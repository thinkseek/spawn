# Spawn

English · [简体中文](./README.zh-CN.md)

Spawn runs Claude Code and Codex CLI agents in isolated workspaces and records everything they do in one PostgreSQL event log.

- A caller submits a command (start, input, resume, stop) to a queue in PostgreSQL.
- `spawn tick` claims the command, forks a git branch, exports it into a working tree and starts the engine in a tmux session.
- Claude Code hooks, Codex JSON output and the terminal tail become typed events in the log.
- An Observer judges each session (ready, working, blocked, failed) and writes the result back as an event.

The log is the only state. Each `spawn tick` is a new process that rebuilds its view from the log, so a crash loses no state.

## Requirements

| Tool | Version | Used for |
| --- | --- | --- |
| Rust | 1.95 or later | Build |
| PostgreSQL | 16 or later | Event log, command queue, views |
| tmux | 3.x | Terminals |
| git | 2.30 or later | Branches and workspaces |
| Claude Code and/or Codex CLI | current | Engines |
| docker | optional | Runs the engine in a container when `SPAWN_EXEC_IMAGE` is set |

## Install

```bash
cargo install --path crates/spawn-cli
```

The binary is `spawn`.

## Quick start

Install the schema:

```bash
export SPAWN_DATABASE_URL=postgres://spawn@localhost/spawn
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

Print the latest observation for each session:

```bash
spawn status
```

Run `spawn tick` on a schedule (cron, systemd timer or a shell loop), or keep `spawn loop` running: it ticks the Bridge and the Observer and keeps the supervisor alive. The RSI controller runs separately as `spawn rsi-loop` (see docs/rsi.md).

## Commands

| Command | What it does |
| --- | --- |
| `init-db` | Installs or updates the SQL schema. Safe to run again. |
| `submit` | Writes one command to the queue. Flags: `--agent`, `--session`, `--key`, `--action`, `--principal` (default `spawn`). |
| `tick` | Claims and runs queued commands, advances live executions, records observations. Flags: `--repo`, `--root`, `--judge-url`. |
| `status` | Prints the latest observation for each session. |
| `loop` | Runs the fleet until killed: Bridge, Observer, dispatcher, and (with `SPAWN_SUPERVISOR=1`) the supervisor's liveness rule. |
| `rsi-loop` | Runs the RSI controller (`evolve` in a loop); exits 2 on HALT. |
| `fleet` | Every session the login may read, with its judgement, as JSON. |
| `tail --session <id>` | One session's latest judgement, recent events, judgements and commands, as JSON. |
| `dispatch [--dry-run] [--root <dir>]` | One dispatcher step; `--dry-run` prints the plan and submits nothing. `--root` is the workspace root (default `SPAWN_WORKSPACE_ROOT`), as for `loop` and `tick`. |
| `corpus export [--since-hours N]` | Model judgements with their evidence and outcome labels, one JSON line each. |
| `judge --replay <file>` | Runs the current rules over a corpus: which tier decides each episode, coverage, agreement. |
| `serve` | Read-only dashboard and API (fleet, judgements, commands, live panes, RSI) as `spawn_reader`. |
| `hook` | Reads one Claude Code hook payload from standard input. Spawn configures this; you do not call it. |
| `run` | Runs the engine inside the terminal and records its exit. Spawn configures this; you do not call it. |

Actions for `--action`:

| `type` | Fields |
| --- | --- |
| `start` | `engine` (`claude` or `codex`), `prompt` (Codex requires a non-empty one), optional `parent_session_id` |
| `input` | `text`; Claude only |
| `resume` | optional `prompt`; Codex requires it |
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
`SPAWN_DATABASE_URL=postgres://.../spawn_test scripts/mvp-e2e.sh`.

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | required | PostgreSQL connection string |
| `SPAWN_GIT_REPO` | required | Bare repository that holds the base commits |
| `SPAWN_WORKSPACE_ROOT` | required | Directory holding one working tree per branch; `--root` overrides it for `tick`, `loop` and `dispatch` |
| `SPAWN_WORKER` | `spawn-worker` | Worker name on claimed commands |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux path |
| `SPAWN_TMUX_SOCKET` | default socket | tmux `-L` socket name |
| `SPAWN_EXEC_IMAGE` | unset | When set, the engine and its tmux run in one container per branch |
| `SPAWN_DOCKER_PROGRAM` | `docker` | docker path |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude Code path |
| `SPAWN_CODEX_PROGRAM` | `codex` | Codex CLI path |
| `SPAWN_PROGRAM` | current binary, else `spawn` | `spawn` path used inside the terminal, in the commands the dispatcher submits, and in the supervisor's charter. When unset: the path of the running executable, or plain `spawn` if that path cannot be determined |
| `SPAWN_JUDGE_URL` | unset | Jev judge service; without it a local heuristic judges |
| `SPAWN_TRACE` | unset | When set, prints the time of each port call to standard error |

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

The PostgreSQL tests need `SPAWN_TEST_DATABASE_URL` set to a disposable database; they do not skip. `scripts/e2e-smoke.sh` runs an end-to-end pass with real PostgreSQL and tmux. `scripts/run-gate-on-remote.sh` runs all checks on a remote host.

## Documentation

| Document | Content |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md) | Architecture overview (Chinese) |
| [docs/design.md](./docs/design.md) | Design, event protocol, port contracts, known limits (Chinese) |
| [docs/view-architecture.md](./docs/view-architecture.md) | Read views (Chinese) |

## License

Apache-2.0
