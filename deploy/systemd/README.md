# systemd user units

The units this host runs, with `%h` for the home directory.

| Unit | Runs | Stops on its own |
| --- | --- | --- |
| `spawn-loop` | `spawn loop`: Bridge, Observer, dispatcher, supervisor and resident liveness | never (`Restart=always`) |
| `spawn-integrate` | `spawn integrate`: lands gated missions' work through `scripts/rsi/gate.sh`, assigns objective tasks (RSI is an objective mission) | never; a halt is per mission, cleared by `spawn mission resume` |
| `spawn-serve` | `spawn serve`: read-only UI and API, as `spawn_reader`; the UI is the build `deploy.sh` installs in `~/.local/share/spawn-rsi/web` (`SPAWN_WEB_DIR`), on `SPAWN_SERVE_ADDRESS` (default `127.0.0.1:8787`). Off loopback (e.g. `0.0.0.0:8787` for the LAN) it refuses to start without `SPAWN_SERVE_TOKEN`; open the UI once as `http://<host>:8787/?token=<token>` | never; without the token it exits and restarts every 3 s |
| `spawn-jev-relay` | `scripts/jev-relay.py` on 127.0.0.1:8790, holding the API key | never |
| `spawn-model-relay` | `scripts/model-relay.py` on 127.0.0.1:8791: the engines' model calls, `deepseek-*` to DeepSeek with `DEEPSEEK_API_KEY` (from `~/.hermes/.env`, held only here), everything else to the gateway of `~/.claude/settings.json` with the engine's own auth ([fleet-architecture.md §3.3](../../docs/fleet-architecture.md#33-engine-models)) | never |
| `spawn-backup` (`.timer`) | `pg-backup.sh`: daily encrypted `pg_dump` to R2 (bucket-locked, R2 expires it); install and keys in [deploy/backup](../backup/README.md) | oneshot, daily at 03:30 + random delay |
| `spawn-maintenance` (`.timer`) | `scripts/maintenance.sh` from the checkout: `spawn chunks gc --grace 120`, the gate's leftover scratch (`$TMPDIR/spawn-gate.XXXXXX` older than a day), deploy.sh's dumps in the backups directory beyond the newest 5; nothing Spawn did not create ([docs/storage.md](../../docs/storage.md#disk-room)) | oneshot, daily at 05:15 + random delay |

`spawn-loop` uses `KillMode=process` so a restart keeps the agents' tmux
server alive. That only works because `ExecStart` is `spawn loop` itself: with
a wrapper script as the main process, systemd would kill the wrapper and leave
the old loop running beside the new one.

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
`openssl rand -hex 32`).

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
cd ~/projects/Spawn   # the checkout the unit runs from, at the merged main
set -a; . ~/.local/share/spawn-rsi/spawn.env; set +a
SPAWN_MAINTENANCE_DRY_RUN=1 sh scripts/maintenance.sh
sh scripts/test-maintenance.sh                    # throwaway directories only
cp deploy/systemd/spawn-maintenance.service deploy/systemd/spawn-maintenance.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-maintenance.timer
systemctl --user start spawn-maintenance.service  # one run now, optional
journalctl --user -u spawn-maintenance -n 20
```

Settings, in `spawn.env`: `SPAWN_MAINTENANCE_KEEP_DUMPS` (5),
`SPAWN_MAINTENANCE_CHUNK_GRACE_MIN` (120), `SPAWN_MAINTENANCE_SCRATCH_AGE_MIN`
(1440), `SPAWN_MAINTENANCE_CHUNKS=0` to skip the chunk collection.

## Deploying

`deploy/deploy.sh <verdict-file>` deploys the checked-out commit, and only one the gate accepted: the verdict file must be the gate's, accepted, with `"candidate"` = HEAD and `"base"` = the missions repo's current `main` (the script's header shows the gate command). Then: backup, release build (no kache) and UI build from a `git archive` export of HEAD (nothing untracked in the checkout is built) inside the bubblewrap sandbox of `scripts/sandbox.sh` (no network, `$HOME` hidden; the target directory `~/.cache/spawn/deploy-target` persists, so builds stay incremental), then with the integrator stopped it migrates the schema with the staged binary and only then swaps binary and UI (`spawn.prev` and `web.prev` kept; a deploy run again with the same build leaves them alone), moves the missions repo's `main` forward (never forced) and records `spawn mission moved`, so gated missions take the new main as their baseline instead of halting as if someone had pushed outside the gate. Between the push and that record it keeps `deploy.pending` in the live directory; a deploy that dies there is completed on its exit or by the next deploy. It takes `deploy.lock` in the live directory, so two deploys never run at once. It refuses a dirty tree, a HEAD that tracks files under a `node_modules/`, a HEAD that does not contain the current `main`, or less than `SPAWN_DEPLOY_MIN_FREE_GB` (default 10) free in `$TMPDIR` or the target directory, and restarts the services if anything fails after it stopped the integrator. `deploy/test-deploy.sh` runs it end to end against a fake live directory. Rolling back to `spawn.prev` is unsafe once the new binary has written an event kind the old one lacks ([docs/upgrading.md](../../docs/upgrading.md#deploying)).
