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

Run `spawn tick` on a schedule (cron, systemd timer or a shell loop). Spawn has no daemon.

## Commands

| Command | What it does |
| --- | --- |
| `init-db` | Installs or updates the SQL schema. Safe to run again. |
| `submit` | Writes one command to the queue. Flags: `--agent`, `--session`, `--key`, `--action`, `--principal` (default `spawn`). |
| `tick` | Claims and runs queued commands, advances live executions, records observations. Flags: `--repo`, `--root`, `--judge-url`. |
| `status` | Prints the latest observation for each session. |
| `hook` | Reads one Claude Code hook payload from standard input. Spawn configures this; you do not call it. |
| `run` | Runs the engine inside the terminal and records its exit. Spawn configures this; you do not call it. |

Actions for `--action`:

| `type` | Fields |
| --- | --- |
| `start` | `engine` (`claude` or `codex`), `prompt` (Codex requires a non-empty one), optional `parent_session_id` |
| `input` | `text`; Claude only |
| `resume` | optional `prompt`; Codex requires it |
| `stop` | none |

## Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | required | PostgreSQL connection string |
| `SPAWN_GIT_REPO` | required | Bare repository that holds the base commits |
| `SPAWN_WORKSPACE_ROOT` | required | Directory holding one working tree per branch |
| `SPAWN_WORKER` | `spawn-worker` | Worker name on claimed commands |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux path |
| `SPAWN_TMUX_SOCKET` | default socket | tmux `-L` socket name |
| `SPAWN_EXEC_IMAGE` | unset | When set, the engine and its tmux run in one container per branch |
| `SPAWN_DOCKER_PROGRAM` | `docker` | docker path |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude Code path |
| `SPAWN_CODEX_PROGRAM` | `codex` | Codex CLI path |
| `SPAWN_PROGRAM` | current binary | `spawn` path used inside the terminal |
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
