# systemd user units

The units this host runs, with `%h` for the home directory.

| Unit | Runs | Stops on its own |
| --- | --- | --- |
| `spawn-loop` | `spawn loop`: Bridge, Observer, dispatcher, supervisor and resident liveness | never (`Restart=always`) |
| `spawn-integrate` | `spawn integrate`: lands gated missions' work through `scripts/rsi/gate.sh`, assigns objective tasks (RSI is an objective mission) | never; a halt is per mission, cleared by `spawn mission resume` |
| `spawn-serve` | `spawn serve`: read-only UI and API, as `spawn_reader`, on `SPAWN_SERVE_ADDRESS` (default `127.0.0.1:8787`). Off loopback (e.g. `0.0.0.0:8787` for the LAN) it refuses to start without `SPAWN_SERVE_TOKEN`; open the UI once as `http://<host>:8787/?token=<token>` | never; without the token it exits and restarts every 3 s |
| `spawn-jev-relay` | `scripts/jev-relay.py` on 127.0.0.1:8790, holding the API key | never |
| `spawn-backup` (`.timer`) | `pg-backup.sh`: daily encrypted `pg_dump` to R2 (bucket-locked, R2 expires it); install and keys in [deploy/backup](../backup/README.md) | oneshot, daily at 03:30 + random delay |

`spawn-loop` uses `KillMode=process` so a restart keeps the agents' tmux
server alive. That only works because `ExecStart` is `spawn loop` itself: with
a wrapper script as the main process, systemd would kill the wrapper and leave
the old loop running beside the new one.

Install: copy to `~/.config/systemd/user/`, then
`systemctl --user daemon-reload && systemctl --user enable --now spawn-loop spawn-integrate spawn-serve spawn-jev-relay`.
Configuration lives in `~/.local/share/spawn-rsi/spawn.env` (KEY=VALUE),
including `SPAWN_SUPERVISOR=1`, `SPAWN_MISSION_FILE`, and `SPAWN_SERVE_ADDRESS`
with `SPAWN_SERVE_TOKEN` when the UI serves the LAN (a URL-safe secret, e.g.
`openssl rand -hex 32`).

## Deploying

`deploy/deploy.sh <verdict-file>` deploys the checked-out commit, and only one the gate accepted: the verdict file must be the gate's, accepted, with `"candidate"` = HEAD and `"base"` = the missions repo's current `main` (the script's header shows the gate command). Then: backup, release build (no kache), binary swap (`spawn.prev` kept), then with the integrator stopped it migrates the schema, moves the missions repo's `main` forward (never forced) and records `spawn mission moved`, so gated missions take the new main as their baseline instead of halting as if someone had pushed outside the gate. It refuses a dirty tree or a HEAD that does not contain the current `main`, and restarts the services if anything fails after it stopped the integrator. Rolling back to `spawn.prev` is unsafe once the new binary has written an event kind the old one lacks ([docs/upgrading.md](../../docs/upgrading.md#deploying)).
