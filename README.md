# Spawn

English · [简体中文](./README.zh-CN.md)

Spawn runs coding agents (Claude Code, and optionally agy) in isolated workspaces and records everything they do in one PostgreSQL event log. The log is the only state: every process rebuilds its view from it, so a crash loses nothing.

## The Spawn ecosystem

| Repository | Role |
| --- | --- |
| [`thinkseek/spawn`](https://github.com/thinkseek/spawn) (this repo) | Rust core: the `spawn` CLI, event log, Bridge/Observer, missions, landing gate, dashboard and HTTP API |
| [`thinkseek/spawn-node`](https://github.com/thinkseek/spawn-node) | Go `spawn-node`: worker-node daemon plus the database-free edge, node-gateway and outbound fronts that talk to `spawn serve --ipc` over a local socket |
| [`thinkseek/spawn-android`](https://github.com/thinkseek/spawn-android) | Android client for the dashboard API: briefing, feature map, nodes, read-only agent views |

## How it works

- A caller queues a command (`start`, `input`, `resume`, `stop`) in PostgreSQL.
- The **Bridge** (`spawn tick` / `spawn loop`) claims it, prepares the agent's workspace and starts the engine in a Herdr terminal (embedded `herdr-core` via the sibling `spawn-hc` binary).
- Transcripts, terminal screens and exit status become typed events. The engine's terminal holds no database login.
- The **Observer** judges each session (ready, working, blocked, failed) and writes the judgement back as an event.
- Above that, **missions** split work into tasks for workers; `spawn integrate` lands accepted work through one gate.

## Core capabilities

| Area | What Spawn provides |
| --- | --- |
| Engines | `claude` (default) and `agy`, enabled with `SPAWN_ENGINES=claude,agy`; a start action or `spawn task assign --engine` picks one |
| Sandbox | Engines without their own deny rules (agy) run only inside a `bwrap` filesystem view (`SPAWN_ENGINE_VIEW=required`, the default); `SPAWN_CLAUDE_SANDBOX=1` puts Claude Code in it too |
| Workspaces | Pluggable backends: `git`, `cow`, `auto`, `overlay`, `d6-shadow`, `d6` (see [Storage](#storage-backends)) |
| Missions & landing | Supervisor plans and assigns tasks; `spawn integrate` judges each candidate with `scripts/rsi/gate.sh` and lands it with a compare-and-swap push |
| Task grading | Three tiers (`l1`/`l2`/`l3`): tasks route to the cheapest preset graded for their difficulty and step up on repeated rework ([docs/agent-grading.md](./docs/agent-grading.md)) |
| Feature map | `spawn features` keeps `docs/feature-map/registry.json` in sync with the code; `check --deps enforce` checks declared feature dependencies against a Rust/TypeScript/Python/Go code graph |
| Nodes | `spawn node` adds, enrolls, drains and revokes machines; `spawn-node` workers join with an enroll token |
| Dashboard & API | `spawn serve`: web UI plus HTTP API ([docs/api.md](./docs/api.md)); `--ipc` serves the same API over a `0600` Unix socket for `spawn-node front` |

## Requirements

| Tool | Version | Used for |
| --- | --- | --- |
| Rust | 1.95 | Build (`rust-toolchain.toml`) |
| PostgreSQL | 16+ | Event log, command queue, views |
| git | 2.43+ (checked at start) | Branches and workspaces |
| Claude Code | current | Default engine |
| `agy`, `bwrap` | optional | Second engine and its sandbox |
| herdr | 0.9.5 (optional) | Only with `SPAWN_HERDR_MODE=legacy` |

## Install

```bash
cargo install --path crates/spawn-cli   # spawn
cargo install --path crates/spawn-hc    # spawn-hc, installed beside spawn
```

## Quick start

```bash
# The password is required: restricted logins derive theirs from it.
export SPAWN_DATABASE_URL=postgres://spawn:PASSWORD@localhost/spawn
spawn init-db

spawn submit --agent agent_demo --session session_demo --key demo-1 \
  --action '{"type":"start","engine":"claude","prompt":"say hello"}'
spawn tick --repo /data/spawn/repo.git --root /data/spawn/ws   # one Bridge + Observer pass
spawn status                                                   # sessions, open tasks, alerts
spawn say agent_demo "please also add a test"                  # type into the live session
```

In production run the long-lived processes instead of `tick`: `spawn loop` (Bridge, Observer, dispatcher; `SPAWN_SUPERVISOR=1` adds the supervisor), `spawn integrate` (gate and landing) and `spawn serve`. Matching user units live in [`deploy/systemd/`](./deploy/systemd/).

## Configuration

Configuration is environment first. A `spawn.toml` can supply the same settings: `spawn config init --data-dir <dir>` writes one, `spawn config check` validates it, and `spawn --config spawn.toml <command>` (or `SPAWN_CONFIG`) applies it. Secrets in it must be `{ from_env = "..." }` or `{ from_file = "..." }`. `spawn doctor` probes the host (kernel, git, herdr, bwrap, …) and the configuration.

| Variable | Default | Meaning |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | required | PostgreSQL connection string |
| `SPAWN_GIT_REPO` | required | Bare repository for agents no mission owns |
| `SPAWN_WORKSPACE_ROOT` | required | Root for workspaces (`--root` overrides) |
| `SPAWN_ENGINES` | `claude` | Enabled engines: `claude`, `agy` |
| `SPAWN_AGY_MODEL` | unset | Model for agy; a start of agy is refused without it |
| `SPAWN_ENGINE_VIEW` | `required` | Sandbox policy: `required`, `container` or `off` |
| `SPAWN_WORKSPACE_BACKEND` | `git` | `git`, `cow`, `auto`, `overlay`, `d6-shadow` or `d6` |
| `SPAWN_HERDR_MODE` | `core` | `core` (sibling `spawn-hc`) or `legacy` (external `herdr`) |
| `SPAWN_JUDGE_URL` | unset | Optional judge service; without it the rules judge alone |
| `SPAWN_SERVE_ADDRESS` | `127.0.0.1:8787` | `spawn serve` bind address |
| `SPAWN_SERVE_TOKEN` | unset | Bearer token; required off loopback |

`SPAWN_TERMINAL_BACKEND` accepts only `herdr`; `tmux` is refused. Every other variable is documented where it is read: [docs/fleet-architecture.md](./docs/fleet-architecture.md), [docs/storage.md](./docs/storage.md), [docs/upgrading.md](./docs/upgrading.md).

## Storage backends

| Backend | What it is |
| --- | --- |
| `git` (default) | A Git working copy per branch |
| `cow` | Copy-on-write clones of a per-commit baseline |
| `auto` | `cow` where the file system clones, else `git` |
| `overlay` | Unprivileged overlay per workspace (Linux, opt-in) |
| `d6-shadow` | `git` stays authoritative; `d6` runs beside it as a checked shadow |
| `d6` | Content-addressed store: SHA-256 objects, FastCDC chunking for large files, Merkle trees, refs in PostgreSQL, optional bounded `tmpfs` layer (`SPAWN_D6_TMPFS_ROOT`) ([docs/d6-cas-v1.md](./docs/d6-cas-v1.md)) |

A workspace root records the backend that claimed it; an incompatible backend is refused. Opt-ins: `SPAWN_CAS_DEDUP=1` deduplicates `target/` and `node_modules/` in Cow workspaces; `SPAWN_SHARED_BUILD_CACHES=1` shares npm, pip, uv, Go, ccache, sccache and zig caches across workspaces.

## Commands

`spawn --help` lists every command; `spawn <command> --help` prints its verbs and flags without opening a store.

| Group | Commands |
| --- | --- |
| Setup & health | `init-db`, `config`, `doctor`, `version` |
| Run the fleet | `tick`, `loop`, `integrate`, `serve [--ipc]` |
| Talk to agents | `submit`, `say`, `ack` |
| Inspect | `status`, `fleet`, `tail`, `alerts`, `alert`, `scorecard` |
| Missions | `mission`, `task` (incl. `task commits`, `task diff`, `task land`), `resident`, `objective`, `verify` |
| Grading & code map | `grade`, `features`, `prov` |
| Nodes & storage | `node`, `backup`, `store`, `source`, `scratch`, `commit-map`, `mask` |

`scripts/fleet-sync-node.sh` pulls a published `spawn-node` (or full Spawn) build onto a node: verifies checksums, swaps binaries, restarts units and rolls back on a failed health check.

## Development

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # PostgreSQL tests need SPAWN_TEST_DATABASE_URL
spawn features check --deps enforce
```

The landing gate runs every check on one tree: `scripts/rsi/gate.sh $PWD/.git HEAD HEAD $PWD/rsi/scenarios` (build, clippy, tests, `scripts/e2e.sh`, black-box scenarios, web build).

## Documentation

| Document | Content |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md), [docs/design.md](./docs/design.md) | Architecture and design (Chinese) |
| [docs/fleet-architecture.md](./docs/fleet-architecture.md) | Observation, control, missions, landing, nodes, alerts |
| [docs/api.md](./docs/api.md) | Every HTTP route and its CLI twin |
| [docs/storage.md](./docs/storage.md), [docs/d6-cas-v1.md](./docs/d6-cas-v1.md) | Storage, workspace backends, CAS format |
| [docs/agent-grading.md](./docs/agent-grading.md) | Task grading and routing |
| [docs/feature-map.md](./docs/feature-map.md) | Feature registry and dependency checks |
| [docs/node-protocol-v1.md](./docs/node-protocol-v1.md), [docs/spawn-ipc-v1.md](./docs/spawn-ipc-v1.md) | `spawn-node` wire protocol and local IPC contract |
| [docs/rsi.md](./docs/rsi.md), [docs/upgrading.md](./docs/upgrading.md), [docs/glossary.md](./docs/glossary.md) | Self-improvement loop (Chinese), upgrades, shared vocabulary |

## License

Apache-2.0
