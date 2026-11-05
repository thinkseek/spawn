# Spawn

English · [简体中文](./README.zh-CN.md)

Spawn runs Claude Code agents in isolated workspaces and records everything they do in one PostgreSQL event log.

- A caller submits a command (start, input, resume, stop) to a queue in PostgreSQL.
- `spawn tick` claims the command, forks a git branch, exports it into a working tree and starts the engine in a tmux session.
- Claude Code's transcript, the terminal screen and the engine's exit status become typed events in the log. The engine's terminal holds no database login.
- An Observer judges each session (ready, working, blocked, failed) and writes the result back as an event.

The log is the only state. Each `spawn tick` is a new process that rebuilds its view from the log, so a crash loses no state.

## Requirements

| Tool | Version | Used for |
| --- | --- | --- |
| Rust | 1.95 or later | Build |
| PostgreSQL | 16 or later | Event log, command queue, views |
| tmux | 3.x | Terminals |
| git | 2.43 or later (`git --attr-source`, checked at start) | Branches and workspaces |
| Claude Code | current | Engine |

## Install

```bash
cargo install --path crates/spawn-cli
```

The binary is `spawn`.

## Quick start

Install the schema:

```bash
# The password is required: the restricted logins (supervisor, dashboard)
# derive theirs from it; without one they cannot log in.
export SPAWN_DATABASE_URL=postgres://spawn:PASSWORD@localhost/spawn
spawn init-db
```

Submit a start command:

```bash
spawn submit --agent agent_demo --session session_demo --key demo-1 --action '{"type":"start","engine":"claude","prompt":"say hello"}'
```

Run one pass of the Bridge and the Observer:

```bash
spawn tick --repo /data/spawn/repo.git --root /data/spawn/ws
```

Print every session with its latest judgement, and one session's events:

```bash
spawn fleet
spawn tail --session session_demo
```

Run `spawn tick` on a schedule (cron, systemd timer or a shell loop), or keep `spawn loop` running: it ticks the Bridge and the Observer and keeps the supervisor alive. Landing and RSI run separately as `spawn integrate` (docs/fleet-architecture.md §10): it lands gated missions' work through the gate, and RSI is an objective mission whose tasks come from `rsi/backlog`.

## Commands

| Command | What it does |
| --- | --- |
| `init-db` | Installs or updates the SQL schema. Safe to run again. |
| `submit` | Writes one command to the queue. Flags: `--agent`, `--session`, `--key`, `--action`, `--principal` (default `spawn`). |
| `tick` | Claims and runs queued commands, advances live executions, records observations. Flags: `--repo`, `--root`, `--judge-url`. |
| `loop` | Runs the fleet until killed: Bridge, Observer, dispatcher, and (with `SPAWN_SUPERVISOR=1`) the supervisor's liveness rule. |
| `integrate [--once] [--root <dir>]` | The one landing path: for each mission with a gate, judges ready work with `scripts/rsi/gate.sh`, lands it on the source with a compare-and-swap push, halts a mission on the halt rules, and gives objective missions (RSI) their next backlog tasks, up to the mission's worker limit (`--workers`, default 1) in flight at once. |
| `fleet` | Every session the login may read, with its judgement, as JSON. |
| `tail --session <id>` | One session's latest judgement, recent events, judgements and commands, as JSON. |
| `serve` | Read-only dashboard and API (fleet, judgements, commands, live panes, RSI) as `spawn_reader`. Binds `127.0.0.1:8787`; on any other address it needs `SPAWN_SERVE_TOKEN`, and every API request then carries it (`Authorization: Bearer <token>` or `?token=<token>`). Open the UI once as `http://<host>:8787/?token=<token>`. |
| `mission`, `task`, `resident` | The mission layer: add, configure, close and resume missions; assign, accept and reject tasks; propose, approve and retire residents. Every command prints JSON. |
| `task stages --id N` | A task's stages from the control stream, oldest first: `[{stage, verdict, at_ms, took_ms}]`, `took_ms` the time since the task's previous stage. `mission list` and the dashboard's missions page carry the same list on every task. |
| `run` | Runs the engine inside the terminal and writes its exit status to a file the Bridge reads. Spawn configures this; you do not call it. |
| `version` | Prints `spawn <version>`. |

Actions for `--action`:

| `type` | Fields |
| --- | --- |
| `start` | `prompt`, optional `engine` (`claude`, the only engine), optional `parent_session_id`, optional `from` (another agent of the same mission source, meaning its branch head, or a full commit id: where a new branch starts instead of `main`) |
| `input` | `text` |
| `resume` | optional `prompt` |
| `stop` | none |

Missions: `spawn mission add --text ... [--source <repo>]` (a human), then the supervisor plans
and routes tasks (`spawn task assign`), workers report in `.spawn/report.md`, the supervisor
accepts or sends back. Worker branches live in the mission's mirror under
`$SPAWN_WORKSPACE_ROOT/.sources/`.

One gate judges every landing, RSI or fleet:
`scripts/rsi/gate.sh <bare repo> <base> <candidate> $PWD/rsi/scenarios [<task scenario>]`.
It refuses changes to the paths in `rsi/scenarios/protected` and to build-time code, builds
each tree with `rsi/scenarios/build`, wants the task scenario red on the base and green on the
candidate, and runs every other non-ratchet scenario on the candidate: clippy, the tests, the
e2e, the black-box CLI scenarios and the web build are scenarios (`rsi/scenarios/check-*.sh`).
Its own tests: `scripts/rsi/test-gate.sh`.

End to end check of the whole decision ladder (scripted engine, stub Jev, ~20 s):
`SPAWN_DATABASE_URL=postgres://.../spawn_test scripts/e2e.sh`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | required | PostgreSQL connection string |
| `SPAWN_GIT_REPO` | required | Bare repository for agents no mission owns (the supervisor, ad hoc workers) |
| `SPAWN_WORKSPACE_ROOT` | required | Directory holding one working tree per branch; `--root` overrides it for `tick` and `loop` |
| `SPAWN_WORKER` | `spawn-worker` | Worker name on claimed commands |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux path |
| `SPAWN_TMUX_SOCKET` | default socket | tmux `-L` socket name |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude Code path |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects` | Where Claude Code writes its transcripts |
| `SPAWN_MAX_FILE_MB` | `100` | Largest file a checkpoint commits, in MiB; a larger one is left out of the checkpoint and the agent is told once |
| `SPAWN_PROGRAM` | current binary, else `spawn` | `spawn` path used inside the terminal, in the commands the dispatcher submits, and in the supervisor's charter. When unset: the path of the running executable, or plain `spawn` if that path cannot be determined |
| `SPAWN_JUDGE_URL` | unset | Jev judge service; without it the rules judge alone, and a case they cannot settle asks for attention |
| `SPAWN_LOOP_INTERVAL_MS` | `500` | Pause between `spawn loop` passes |
| `SPAWN_SERVE_ADDRESS` | `127.0.0.1:8787` | Address `spawn serve` binds (`--address` overrides it) |
| `SPAWN_SERVE_TOKEN` | unset | Token every `spawn serve` API request must carry; required off loopback |

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The PostgreSQL tests need `SPAWN_TEST_DATABASE_URL` set to a disposable database; they do not skip. All checks on one tree, through the same gate that lands work: `scripts/rsi/gate.sh $PWD/.git HEAD HEAD $PWD/rsi/scenarios` (build, clippy, tests, `scripts/e2e.sh`, the black-box scenarios in `scripts/blackbox/`, the web build).

## Documentation

| Document | Content |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md) | Architecture overview (Chinese) |
| [docs/design.md](./docs/design.md) | Design, event protocol, port contracts, known limits (Chinese) |
| [docs/view-architecture.md](./docs/view-architecture.md) | Read views (Chinese) |
| [docs/fleet-architecture.md](./docs/fleet-architecture.md) | The fleet: observation, control, missions, landing |
| [docs/storage.md](./docs/storage.md) | Where each kind of object is stored; branching and merging |
| [docs/rsi.md](./docs/rsi.md) | RSI: the objective mission and its gate (Chinese) |
| [docs/upgrading.md](./docs/upgrading.md) | Replacing processes of a running Spawn |

## License

Apache-2.0
