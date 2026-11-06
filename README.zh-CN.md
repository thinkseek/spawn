# Spawn

[English](./README.md) · 简体中文

Spawn 在隔离的工作区中运行 Claude Code agent，并把它们的全部行为记录到一个 PostgreSQL 事件日志。

- 调用方向 PostgreSQL 中的队列提交命令（start、input、resume、stop）。
- `spawn tick` 领取命令，创建一个 git 分支，导出工作树，在 tmux 会话中启动引擎。
- Claude Code 的 transcript、终端屏幕与引擎退出状态转为日志中的类型化事件。引擎所在终端不持有数据库登录。
- Observer 判定每个会话的状态（可输入、运行中、阻塞、失败），并把结论作为事件写回。

日志是唯一的状态。每次 `spawn tick` 是新进程，从日志重建视图，进程崩溃不丢状态。

## 依赖

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Rust | 1.95 及以上 | 编译 |
| PostgreSQL | 16 及以上 | 事件日志、命令队列、视图 |
| tmux | 3.x | 终端 |
| git | 2.40 及以上（`git --attr-source`，启动时检查） | 分支与工作区 |
| Claude Code | 当前版本 | 引擎 |

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

打印每个会话及其最新判定，以及单个会话的事件：

```bash
spawn fleet
spawn tail --session session_demo
```

生产部署跑常驻进程：`spawn loop`（Bridge、Observer、派发、存活规则，默认 500 ms 一轮）、`spawn integrate`（门禁与落地）、`spawn serve`（只读看板），对应 `deploy/systemd/` 下的 user unit。`spawn tick` 只跑一轮，用于调试和测试。

## 命令

| 命令 | 作用 |
| --- | --- |
| `init-db` | 安装或更新 SQL schema，可重复执行 |
| `submit` | 向队列写一条命令。参数：`--agent`、`--session`、`--key`、`--action`、`--principal`（默认 `spawn`） |
| `tick` | 领取并执行排队命令，推进活跃执行，记录判定。参数：`--repo`、`--root`、`--judge-url` |
| `loop` | 常驻运行舰队：Bridge、Observer、派发，以及（`SPAWN_SUPERVISOR=1` 时）supervisor 的存活规则 |
| `fleet` | 以 JSON 打印当前登录可读的每个会话及其判定 |
| `tail --session <id>` | 以 JSON 打印单个会话的最新判定、近期事件、判定与命令 |
| `integrate [--once] [--root <dir>] [--gates <n>]` | 唯一的落地路径：用 `scripts/rsi/gate.sh` 判定就绪的工作（一次一个门禁：每个门禁有自己的测试数据库服务器之前，`--gates` 大于 1 会被拒绝，见 `SPAWN_TEST_DATABASE_PER_GATE=1`），以 compare-and-swap 推送落到来源，按停机规则暂停任务，并给目标任务（RSI）分配下一条 backlog |
| `serve` | 以 `spawn_reader` 提供只读看板与 API（含 `/api/attention`：派发器自己的升级项与 supervisor 待人回答的问题）；默认绑定 `127.0.0.1:8787`，其他地址需要 `SPAWN_SERVE_TOKEN`；`SPAWN_SERVE_OPEN_NETWORKS`（逗号分隔的 CIDR，如内网 `192.168.31.0/24`）内的客户端用该网段内的 IP 地址访问时免 token |
| `scorecard` | 只读记录，一次量出全队、每个 mission、每天的八个维度（吞吐、前导时间、一次通过率、返工、自主、可靠、成本、门禁耗时），带 n、方向、上一窗口与趋势；不足 3 次落地时写 `not enough data`（docs/design.md §15） |
| `mission`、`task`、`resident` | 任务层：添加、配置、关闭、恢复任务；分派、验收、退回子任务；提议、批准、退役常驻 worker。每条命令输出 JSON |
| `run` | 在终端内运行引擎，把退出状态写入文件供 Bridge 读取。由 Spawn 配置，无需手动调用 |
| `version` | 打印 `spawn <版本>` |

`--action` 的取值：

| `type` | 字段 |
| --- | --- |
| `start` | `prompt`、可选 `engine`（`claude`，唯一的引擎）、可选 `parent_session_id`、可选 `from`（同一任务源的另一个 agent，即其分支头，或完整 commit id：新分支从这里开始而不是 `main`） |
| `input` | `text` |
| `resume` | 可选 `prompt` |
| `stop` | 无 |

## 配置

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PostgreSQL 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 不属于任何任务的 agent（supervisor、临时 worker）用的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 每个分支一份工作树的根目录 |
| `SPAWN_WORKER` | `spawn-worker` | 领取命令时记录的 worker 名 |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux 路径 |
| `SPAWN_TMUX_SOCKET` | 默认 socket | tmux `-L` socket 名 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude Code 路径 |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`，否则 `~/.claude/projects` | Claude Code 写 transcript 的目录 |
| `SPAWN_MAX_FILE_MB` | `100` | checkpoint 收录的单个文件上限（MiB）；更大的文件不进 checkpoint，并告知 agent 一次 |
| `SPAWN_PROGRAM` | 当前二进制 | 终端内使用的 `spawn` 路径 |
| `SPAWN_JUDGE_URL` | 未设置 | Jev 判定服务；未设置时只用规则判定，规则定不了的情形要求关注 |

## 开发

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

PostgreSQL 测试需要 `SPAWN_TEST_DATABASE_URL` 指向一个可丢弃的数据库，缺失时测试失败而非跳过。对一棵树跑全部检查，用的就是落地时的同一个门禁：`scripts/rsi/gate.sh $PWD/.git HEAD HEAD $PWD/rsi/scenarios`（构建、clippy、测试、`scripts/e2e.sh`、`scripts/blackbox/` 黑盒场景、web 构建）。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md) | 架构总览 |
| [docs/design.md](./docs/design.md) | 技术方案、事件协议、端口契约、已知限制 |
| [docs/view-architecture.md](./docs/view-architecture.md) | 读视图设计 |
| [docs/fleet-architecture.md](./docs/fleet-architecture.md) | 舰队：判定、控制、任务、落地（英文） |
| [docs/storage.md](./docs/storage.md) | 各类对象的存放位置，分支与合并（英文） |
| [docs/rsi.md](./docs/rsi.md) | RSI：目标任务与门禁 |
| [docs/upgrading.md](./docs/upgrading.md) | 运行中替换各进程（英文） |

## 许可证

Apache-2.0
