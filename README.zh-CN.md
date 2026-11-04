# Spawn

[English](./README.md) · 简体中文

Spawn 在隔离的工作区中运行 Claude Code 与 Codex CLI agent，并把它们的全部行为记录到一个 PostgreSQL 事件日志。

- 调用方向 PostgreSQL 中的队列提交命令（start、input、resume、stop）。
- `spawn tick` 领取命令，创建一个 git 分支，导出工作树，在 tmux 会话中启动引擎。
- Claude Code 的 transcript、Codex JSON 输出、终端屏幕与引擎退出状态转为日志中的类型化事件。引擎所在终端不持有数据库登录。
- Observer 判定每个会话的状态（可输入、运行中、阻塞、失败），并把结论作为事件写回。

日志是唯一的状态。每次 `spawn tick` 是新进程，从日志重建视图，进程崩溃不丢状态。

## 依赖

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Rust | 1.95 及以上 | 编译 |
| PostgreSQL | 16 及以上 | 事件日志、命令队列、视图 |
| tmux | 3.x | 终端 |
| git | 2.30 及以上 | 分支与工作区 |
| docker | 可选 | 设置 `SPAWN_EXEC_IMAGE` 时在容器内运行引擎 |
| Claude Code 和/或 Codex CLI | 当前版本 | 引擎 |

## 安装

```bash
cargo install --path crates/spawn-cli
```

二进制名为 `spawn`。

## 快速开始

安装 schema：

```bash
# 必须带密码：受限登录（supervisor、看板）的密码由它派生；没有密码时它们无法登录。
export SPAWN_DATABASE_URL=postgres://spawn:PASSWORD@localhost/spawn
spawn init-db
```

提交 start 命令：

```bash
spawn submit --agent agent_demo --session session_demo --key demo-1 --action '{"type":"start","engine":"claude","prompt":"say hello"}'
```

运行一轮 Bridge 与 Observer：

```bash
spawn tick --repo /data/spawn/repo.git --root /data/spawn/ws
```

打印每个会话的最新判定：

```bash
spawn status
```

生产部署跑常驻进程：`spawn loop`（Bridge、Observer、派发、存活规则，默认 500 ms 一轮）、`spawn integrate`（门禁与落地）、`spawn serve`（只读看板），对应 `deploy/systemd/` 下的 user unit。`spawn tick` 只跑一轮，用于调试和测试。

## 命令

| 命令 | 作用 |
| --- | --- |
| `init-db` | 安装或更新 SQL schema，可重复执行 |
| `submit` | 向队列写一条命令。参数：`--agent`、`--session`、`--key`、`--action`、`--principal`（默认 `spawn`） |
| `tick` | 领取并执行排队命令，推进活跃执行，记录判定。参数：`--repo`、`--root`、`--judge-url` |
| `status` | 打印每个会话的最新判定 |
| `hook` | 读取并忽略标准输入。仅为旧版本以 hook 启动、仍在运行的引擎保留；Spawn 不再配置它 |
| `run` | 在终端内运行引擎，把退出状态写入文件供 Bridge 读取。由 Spawn 配置，无需手动调用 |

`--action` 的取值：

| `type` | 字段 |
| --- | --- |
| `start` | `engine`（`claude` 或 `codex`）、`prompt`（Codex 必须非空）、可选 `parent_session_id`、可选 `from`（同一任务源的另一个 agent，即其分支头，或完整 commit id：新分支从这里开始而不是 `main`） |
| `input` | `text`；仅 Claude |
| `resume` | 可选 `prompt`；Codex 必填 |
| `stop` | 无 |

## 配置

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PostgreSQL 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 存放基线 commit 的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 每个分支一份工作树的根目录 |
| `SPAWN_WORKER` | `spawn-worker` | 领取命令时记录的 worker 名 |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux 路径 |
| `SPAWN_TMUX_SOCKET` | 默认 socket | tmux `-L` socket 名 |
| `SPAWN_EXEC_IMAGE` | 未设置 | 设置后每个分支一个容器，引擎与 tmux 在里面运行 |
| `SPAWN_DOCKER_PROGRAM` | `docker` | docker 路径 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude Code 路径 |
| `SPAWN_CODEX_PROGRAM` | `codex` | Codex CLI 路径 |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`，否则 `~/.claude/projects` | Claude Code 写 transcript 的目录（Bridge 所见路径） |
| `SPAWN_PROGRAM` | 当前二进制 | 终端内使用的 `spawn` 路径 |
| `SPAWN_JUDGE_URL` | 未设置 | Jev 判定服务；未设置时用本地启发式判定 |

## 开发

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

PostgreSQL 测试需要 `SPAWN_TEST_DATABASE_URL` 指向一个可丢弃的数据库，缺失时测试失败而非跳过。对一棵树跑全部检查，用的就是落地时的同一个门禁：`scripts/rsi/gate.sh $PWD/.git HEAD HEAD $PWD/rsi/scenarios`（构建、clippy、测试、`scripts/mvp-e2e.sh`、`scripts/blackbox/` 黑盒场景、web 构建）。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md) | 架构总览 |
| [docs/design.md](./docs/design.md) | 技术方案、事件协议、端口契约、已知限制 |
| [docs/view-architecture.md](./docs/view-architecture.md) | 读视图设计 |

## 许可证

Apache-2.0
