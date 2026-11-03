# systemd user units

The units this host runs, with `%h` for the home directory.

| Unit | Runs | Stops on its own |
| --- | --- | --- |
| `spawn-loop` | `spawn loop`: Bridge, Observer, supervisor liveness | never (`Restart=always`) |
| `spawn-rsi` | `spawn rsi-loop`: the RSI controller | on HALT (exit 2), for a human |
| `spawn-serve` | `spawn serve`: read-only UI and API on the LAN, as `spawn_reader` | never |
| `spawn-jev-relay` | `scripts/jev-relay.py` on 127.0.0.1:8790, holding the API key | never |

`spawn-loop` uses `KillMode=process` so a restart keeps the agents' tmux
server alive. That only works because `ExecStart` is `spawn loop` itself: with
a wrapper script as the main process, systemd would kill the wrapper and leave
the old loop running beside the new one.

Install: copy to `~/.config/systemd/user/`, then
`systemctl --user daemon-reload && systemctl --user enable --now spawn-loop spawn-rsi spawn-serve spawn-jev-relay`.
Configuration lives in `~/.local/share/spawn-rsi/spawn.env` (KEY=VALUE),
including `SPAWN_SUPERVISOR=1` and `SPAWN_MISSION_FILE`.
