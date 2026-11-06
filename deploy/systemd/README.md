# systemd user units

The units this host runs, with `%h` for the home directory.

**An owner edit is live only after its deploy.** No unit runs or reads
anything from the owner's checkout (`~/projects/Spawn`): binaries and UI come
from `~/.local/share/spawn-rsi` (installed by `deploy/deploy.sh`), and the
gate, its scenarios, the objective backlog and the maintenance script from
the controller, the deployed commit's tree (below). Editing `gate.sh`, a
scenario or `rsi/backlog` in the checkout, or merging it to the checkout's
main, changes nothing live until that commit is gated and deployed.

| Unit | Runs | Stops on its own |
| --- | --- | --- |
| `spawn-loop` | `spawn loop`: Bridge, Observer, dispatcher, supervisor and resident liveness | never (`Restart=always`) |
| `spawn-integrate` | `spawn integrate`: lands gated missions' work through the controller's `scripts/rsi/gate.sh` (below), assigns objective tasks (RSI is an objective mission) | never; a halt is per mission, cleared by `spawn mission resume` |
| `spawn-serve` | `spawn serve`: read-only UI and API, as `spawn_reader`; the UI is the build `deploy.sh` installs in `~/.local/share/spawn-rsi/web` (`SPAWN_WEB_DIR`), on `SPAWN_SERVE_ADDRESS` (default `127.0.0.1:8787`). Off loopback (e.g. `0.0.0.0:8787` for the LAN) it refuses to start without `SPAWN_SERVE_TOKEN`; open the UI once as `http://<host>:8787/?token=<token>` | never; without the token it exits and restarts every 3 s |
| `spawn-jev-relay` | `scripts/jev-relay.py` on 127.0.0.1:8790, holding the API key | never |
| `spawn-model-relay` | `scripts/model-relay.py` on 127.0.0.1:8791: the engines' model calls, `deepseek-*` to DeepSeek with `DEEPSEEK_API_KEY` (from `~/.hermes/.env`, held only here), everything else to the gateway of `~/.claude/settings.json` with the engine's own auth ([fleet-architecture.md §3.3](../../docs/fleet-architecture.md#33-engine-models)) | never |
| `spawn-backup` (`.timer`) | `pg-backup.sh`: daily encrypted `pg_dump` to R2 (bucket-locked, R2 expires it); install and keys in [deploy/backup](../backup/README.md) | oneshot, daily at 03:30 + random delay |
| `spawn-maintenance` (`.timer`) | `scripts/maintenance.sh` from the controller: the gate's leftover scratch (`$TMPDIR/spawn-gate.XXXXXX` older than 3 h), deploy.sh's dumps in the backups directory beyond the newest 5; nothing Spawn did not create ([docs/storage.md](../../docs/storage.md#disk-room)) | oneshot, hourly (scratch older than 3 h) |

`spawn-loop` uses `KillMode=process` so a restart keeps the agents' tmux
server alive. That only works because `ExecStart` is `spawn loop` itself: with
a wrapper script as the main process, systemd would kill the wrapper and leave
the old loop running beside the new one. SIGTERM (a stop or a deploy's
restart) ends `spawn loop` after its current pass, once the branch pushes
still queued in its memory are made; `TimeoutStopSec` leaves room for that.
A loop killed outright loses none either: the next one publishes again the
branch of every agent with an open task.

Install: copy to `~/.config/systemd/user/`, then
`systemctl --user daemon-reload && systemctl --user enable --now spawn-loop spawn-integrate spawn-serve spawn-jev-relay spawn-model-relay`.

The relays' scripts run from `~/.local/share/spawn-rsi/bin/`, installed by
hand the first time; after that `deploy.sh` installs a changed relay script and try-restarts its unit (a unit that is not installed is skipped). The model relay:

```sh
install -m 755 scripts/model-relay.py ~/.local/share/spawn-rsi/bin/
python3 -I scripts/model-relay-test.py        # self-test, loopback only
cp deploy/systemd/spawn-model-relay.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-model-relay
curl -s http://127.0.0.1:8791/healthz         # {"ok":true}
journalctl --user -u spawn-model-relay -n 5   # the upstream hosts it routes to
```

Then point the loop at it: `SPAWN_MODEL_RELAY=http://127.0.0.1:8791` in
`spawn.env`, and restart `spawn-loop` (agents keep running,
`KillMode=process`). Unset or `off`, engines run on Claude Code's own
settings and cheap tasks fail. The loop checks the relay before every Claude
start: while it is down, default engines start without it (a tick warning)
and cheap starts fail; engines already running keep the settings they
started with. `SPAWN_CHEAP_MODEL` (default `deepseek-flash`) and
`SPAWN_SMALL_MODEL` (default `deepseek-flash`) go in `spawn.env` too. The relay's gateway is `MODEL_RELAY_UPSTREAM`, else the
`env.ANTHROPIC_BASE_URL` of `~/.claude/settings.json` read at start.
Configuration lives in `~/.local/share/spawn-rsi/spawn.env` (KEY=VALUE),
including `SPAWN_SUPERVISOR=1`, `SPAWN_MISSION_FILE`, and `SPAWN_SERVE_ADDRESS`
with `SPAWN_SERVE_TOKEN` when the UI serves the LAN (a URL-safe secret, e.g.
`openssl rand -hex 32`). `SPAWN_SERVE_OPEN_NETWORKS` (e.g. `192.168.31.0/24`)
lets the owner's intranet read without the token, by the server's address
(`http://192.168.31.112:8787/`); a client elsewhere (Tailscale, a container)
still needs it.

## Disk room

Three guards keep a full disk from corrupting state or wedging the fleet
([docs/storage.md](../../docs/storage.md#disk-room)); the first two ship in
the binary and the checkout, the third is a timer to install.

- The gate exits 3 (an abort, retried) with less than
  `SPAWN_GATE_MIN_FREE_GB` (default 10) GiB free in `TMPDIR`.
- `spawn loop` records a `host.disk` warning when the workspace root or
  `TMPDIR` falls under `SPAWN_DISK_WARN_GB` (default 20) GiB free, and starts
  no new executions under `SPAWN_DISK_FLOOR_GB` (default 2). The fleet view's
  health strip shows free space now.
- `spawn-maintenance.timer`, installed by hand. Look first at what it would
  remove (it deletes nothing in a dry run):

```sh
set -a; . ~/.local/share/spawn-rsi/spawn.env; set +a
SPAWN_MAINTENANCE_DRY_RUN=1 sh ~/.local/share/spawn-rsi/controllers/spawn/scripts/maintenance.sh
sh scripts/test-maintenance.sh                    # in a checkout; throwaway directories only
cp deploy/systemd/spawn-maintenance.service deploy/systemd/spawn-maintenance.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-maintenance.timer
systemctl --user start spawn-maintenance.service  # one run now, optional
journalctl --user -u spawn-maintenance -n 20
```

Settings, in `spawn.env`: `SPAWN_MAINTENANCE_KEEP_DUMPS` (5),
`SPAWN_MAINTENANCE_SCRATCH_AGE_MIN` (1440).

## Deploying

**Hold, then gate, then deploy.** Before the owner gate, run
`deploy/hold.sh [<candidate>]` (default: the checkout's HEAD) instead of
stopping `spawn-integrate`. It records the owner's hold on the missions repo
(`spawn mission hold --source ~/.local/share/spawn-rsi/missions/spawn.git
--reason "owner gate <candidate>"`): no task lands on Spawn's `main` while the
gate judges against it, so the verdict's base stays the live `main` and the
deploy accepts it (on 2026-11-06 at 21:45 a landing during the owner's gate
made the verdict stale). `spawn-integrate` keeps running: every mission's
gates go on (a Spawn task the gate accepts waits to land), objective
assignment goes on (stopping the integrator held Supply PA's new objectives
for the whole deploy), and so do the other sources. The script prints the
hold, the `main` it was taken at and the gate command over that `main`. Then
run the gate and `deploy/deploy.sh <verdict-file>`, which releases the hold on
every exit path once it holds the deploy lock: after a success, once `main`
moved forward and `spawn mission moved` recorded it (the tasks that waited
land at the next pass, judged again on the new `main`), and after any failure.
A hold nobody releases (a deploy given up, a crash) expires by itself after
`SPAWN_SOURCE_HOLD_SECS` (3 h by default) and the supervisor is told;
`spawn mission release --source <that repo>` ends it at once, and
`spawn mission list` shows it (`holds`). The deploy itself still stops the
integrator for its few seconds of install. The first deploy of the hold
itself has no hold to take (the live binary predates it): stop
`spawn-integrate` for that gate as before.

`deploy/deploy.sh <verdict-file>` deploys the checked-out commit, and only one the gate accepted: the verdict file must be the gate's, accepted, with `"candidate"` = HEAD and `"base"` = the missions repo's current `main` (the script's header shows the gate command). Then: backup, release build (no kache) and UI build from a `git archive` export of HEAD (nothing untracked in the checkout is built) inside the bubblewrap sandbox of `scripts/sandbox.sh` (no network, `$HOME` hidden; the target directory `~/.cache/spawn/deploy-target` persists, so builds stay incremental), then with the integrator stopped it migrates the schema with the staged binary and only then swaps binary and UI (`spawn.prev` and `web.prev` kept; a deploy run again with the same build leaves them alone), switches the controller to HEAD's tree (below), moves the missions repo's `main` forward (never forced) and records `spawn mission moved`, so gated missions take the new main as their baseline instead of halting as if someone had pushed outside the gate. Between the push and that record it keeps `deploy.pending` in the live directory; a deploy that dies there is completed on its exit or by the next deploy. It takes `deploy.lock` in the live directory, so two deploys never run at once. It refuses a dirty tree, a HEAD that tracks files under a `node_modules/`, a HEAD that does not contain the current `main`, or less than `SPAWN_DEPLOY_MIN_FREE_GB` (default 10) free in `$TMPDIR` or the target directory, and restarts the services if anything fails after it stopped the integrator. `deploy/test-deploy.sh` runs it end to end against a fake live directory. It releases the owner's hold on the missions repo on every exit path (above). Rolling back to `spawn.prev` goes with `deploy/controller.sh switch spawn <the previous commit>` (`controllers/spawn.prev` names it), and is unsafe once the new binary has written an event kind the old one lacks ([docs/upgrading.md](../../docs/upgrading.md#deploying)).

## The controller

`~/.local/share/spawn-rsi/controllers/<name>` is a symlink to
`<name>-<commit>`, that commit's `git archive`, read-only, with what the gate
needs from a controller that git does not hold (for Spawn, `web/node_modules`,
which `check-web.sh` builds the UI with: a reflink copy of the deploying
checkout's, a few MB of metadata on btrfs; refused unless it was installed from
the commit's lockfile). `deploy/controller.sh` builds one and switches the
symlink atomically (rename over it); the one it replaces stays as
`<name>.prev`, and older ones are removed once no process uses them. The
integrator resolves the symlink once per step (gate and scenarios), so a gate
runs on one commit's files even across a switch. `deploy/test-controller.sh`
tests the switch (scratch directories only).

| Controller | Commit | Read by | Moved by |
| --- | --- | --- | --- |
| `spawn` | the deployed commit (the missions repo's `main` after a deploy) | `spawn-integrate` (`SPAWN_GATE`), `spawn-maintenance`, mission 4's `--gate .../rsi/scenarios` and `--backlog .../rsi/backlog` | `deploy/deploy.sh`, with the install |
| `supply-pa` | the live `main` of `missions/supply-pa.git` | mission 6's `--gate .../gate/scenarios` and `--backlog .../gate/backlog` (the gate script is Spawn's) | the owner's push to that repo, below |

Supply PA's scenarios still name the owner's installed dependencies in
`~/projects/supply-pa` (`gate/scenarios/bind`, `SUPPLY_PA_DEPS`): those are
`pnpm install` output, not tracked files, and its manifests and lockfile are
protected, so only an owner install changes them.

An owner push to a gated source other than Spawn (Supply PA): stop
`spawn-integrate`, push to `~/.local/share/spawn-rsi/missions/<repo>.git`, run
`spawn mission moved --source <that repo> --from <old> --to <new>`, then

```sh
deploy/controller.sh update supply-pa ~/.local/share/spawn-rsi/missions/supply-pa.git main
```

and start `spawn-integrate` again. Until that update, the mission keeps
judging with the scenarios and backlog of the commit before.

The move to controllers, once per host: `deploy/cutover-controller.sh
--dry-run` shows what it would do (nothing changes), then
`deploy/cutover-controller.sh` builds both controllers at the live commits,
installs these units, reconfigures missions 4 and 6 to the controller paths
(every other setting as it is) and restarts `spawn-integrate` once no gate
runs. Deploy this change first (so the integrator pins the controller and
reads tasks' scenarios from the mission's directory), then run the cutover;
it is safe to run again.
