# Spawn

[English](./README.md) · 简体中文

Spawn 在隔离的工作区中运行编码 agent（Claude Code，可选 agy），并把它们的全部行为记录到一个 PostgreSQL 事件日志。日志是唯一的状态：每个进程都从日志重建视图，进程崩溃不丢任何东西。

## Spawn 生态

| 仓库 | 角色 |
| --- | --- |
| [`thinkseek/spawn`](https://github.com/thinkseek/spawn)（本仓库） | Rust 核心：`spawn` CLI、事件日志、Bridge/Observer、mission、落地门禁、看板与 HTTP API |
| [`thinkseek/spawn-node`](https://github.com/thinkseek/spawn-node) | Go `spawn-node`：工作节点守护进程，以及不接触数据库的 edge、节点网关、出站前端，经本地 socket 连接 `spawn serve --ipc` |
| [`thinkseek/spawn-android`](https://github.com/thinkseek/spawn-android) | 看板 API 的 Android 客户端：简报、Feature Map、节点、只读 agent 视图 |

## 工作原理

- 调用方向 PostgreSQL 队列提交命令（`start`、`input`、`resume`、`stop`）。
- **Bridge**（`spawn tick` / `spawn loop`）领取命令，准备 agent 的工作区，在 Herdr 终端中启动引擎（经同级二进制 `spawn-hc` 内置 `herdr-core`）。
- transcript、终端屏幕与退出状态转为类型化事件。引擎所在终端不持有数据库登录。
- **Observer** 判定每个会话的状态（可输入、运行中、阻塞、失败），并把判定作为事件写回。
- 在此之上，**mission** 把工作拆成任务交给 worker；`spawn integrate` 经同一个门禁落地已验收的工作。

## 核心能力

| 领域 | Spawn 提供什么 |
| --- | --- |
| 引擎 | `claude`（默认）与 `agy`，用 `SPAWN_ENGINES=claude,agy` 启用；由 start 动作或 `spawn task assign --engine` 选择 |
| 沙箱 | 自身没有拒绝规则的引擎（agy）只在 `bwrap` 文件系统视图中运行（`SPAWN_ENGINE_VIEW=required`，默认）；`SPAWN_CLAUDE_SANDBOX=1` 让 Claude Code 也进入该视图 |
| 工作区 | 可插拔后端：`git`、`cow`、`auto`、`overlay`、`d6-shadow`、`d6`（见[存储后端](#存储后端)） |
| Mission 与落地 | supervisor 规划并分派任务；`spawn integrate` 用 `scripts/rsi/gate.sh` 判定每个候选，以 compare-and-swap 推送落地 |
| 任务分级 | 三级（`l1`/`l2`/`l3`）：任务路由到按难度分级的最便宜 preset，返工屡次不过时升级（[docs/agent-grading.md](./docs/agent-grading.md)） |
| Feature Map | `spawn features` 让 `docs/feature-map/registry.json` 与代码保持同步；`check --deps enforce` 用 Rust/TypeScript/Python/Go 代码图核对声明的功能依赖 |
| 节点 | `spawn node` 添加、注册、排空、吊销机器；`spawn-node` worker 凭 enroll token 加入 |
| 看板与 API | `spawn serve`：网页 UI 与 HTTP API（[docs/api.md](./docs/api.md)）；`--ipc` 经 `0600` Unix socket 为 `spawn-node front` 提供同一 API |

## 依赖

| 工具 | 版本 | 用途 |
| --- | --- | --- |
| Rust | 1.95 | 编译（`rust-toolchain.toml`） |
| PostgreSQL | 16+ | 事件日志、命令队列、视图 |
| git | 2.43+（启动时检查） | 分支与工作区 |
| Claude Code | 当前版本 | 默认引擎 |
| `agy`、`bwrap` | 可选 | 第二引擎及其沙箱 |
| herdr | 0.9.5（可选） | 仅用于 `SPAWN_HERDR_MODE=legacy` |

## 安装

```bash
cargo install --path crates/spawn-cli   # spawn
cargo install --path crates/spawn-hc    # spawn-hc，安装在 spawn 同目录
```

## 快速开始

```bash
# 必须带密码：受限登录的密码由它派生。
export SPAWN_DATABASE_URL=postgres://spawn:PASSWORD@localhost/spawn
spawn init-db

spawn submit --agent agent_demo --session session_demo --key demo-1 \
  --action '{"type":"start","engine":"claude","prompt":"say hello"}'
spawn tick --repo /data/spawn/repo.git --root /data/spawn/ws   # 一轮 Bridge + Observer
spawn status                                                   # 会话、未完成任务、告警
spawn say agent_demo "please also add a test"                  # 向运行中的会话输入
```

生产环境用常驻进程代替 `tick`：`spawn loop`（Bridge、Observer、派发；`SPAWN_SUPERVISOR=1` 加上 supervisor）、`spawn integrate`（门禁与落地）和 `spawn serve`。对应的 user unit 在 [`deploy/systemd/`](./deploy/systemd/)。

## 配置

配置以环境变量为先。`spawn.toml` 可提供同样的设置：`spawn config init --data-dir <dir>` 生成，`spawn config check` 校验，`spawn --config spawn.toml <命令>`（或 `SPAWN_CONFIG`）应用。其中的密钥必须写成 `{ from_env = "..." }` 或 `{ from_file = "..." }`。`spawn doctor` 探测主机（内核、git、herdr、bwrap 等）与配置。

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PostgreSQL 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 不属于任何 mission 的 agent 用的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 工作区根目录（`--root` 覆盖） |
| `SPAWN_ENGINES` | `claude` | 启用的引擎：`claude`、`agy` |
| `SPAWN_AGY_MODEL` | 不设 | agy 的模型；不设时拒绝启动 agy |
| `SPAWN_ENGINE_VIEW` | `required` | 沙箱策略：`required`、`container` 或 `off` |
| `SPAWN_WORKSPACE_BACKEND` | `git` | `git`、`cow`、`auto`、`overlay`、`d6-shadow` 或 `d6` |
| `SPAWN_HERDR_MODE` | `core` | `core`（同级 `spawn-hc`）或 `legacy`（外部 `herdr`） |
| `SPAWN_JUDGE_URL` | 不设 | 可选判定服务；不设时只用规则判定 |
| `SPAWN_SERVE_ADDRESS` | `127.0.0.1:8787` | `spawn serve` 绑定地址 |
| `SPAWN_SERVE_TOKEN` | 不设 | Bearer token；非回环地址必填 |

`SPAWN_TERMINAL_BACKEND` 只接受 `herdr`，`tmux` 会被拒绝。其余变量在读取它们的文档中说明：[docs/fleet-architecture.md](./docs/fleet-architecture.md)、[docs/storage.md](./docs/storage.md)、[docs/upgrading.md](./docs/upgrading.md)。

## 存储后端

| 后端 | 说明 |
| --- | --- |
| `git`（默认） | 每个分支一份 Git 工作副本 |
| `cow` | 按提交基线做写时复制克隆 |
| `auto` | 文件系统支持克隆时用 `cow`，否则 `git` |
| `overlay` | 每个工作区一个非特权 overlay（Linux，需显式启用） |
| `d6-shadow` | `git` 仍为权威；`d6` 作为受校验的影子并行运行 |
| `d6` | 内容寻址存储：SHA-256 对象、大文件 FastCDC 分块、Merkle 树、ref 存于 PostgreSQL、可选有界 `tmpfs` 层（`SPAWN_D6_TMPFS_ROOT`）（[docs/d6-cas-v1.md](./docs/d6-cas-v1.md)） |

工作区根目录会记录占用它的后端；不兼容的后端会被拒绝。可选项：`SPAWN_CAS_DEDUP=1` 对 Cow 工作区的 `target/` 与 `node_modules/` 去重；`SPAWN_SHARED_BUILD_CACHES=1` 让各工作区共享 npm、pip、uv、Go、ccache、sccache 与 zig 缓存。

## 命令

`spawn --help` 列出全部命令；`spawn <命令> --help` 打印该命令的动词与参数，不打开数据库。

| 分组 | 命令 |
| --- | --- |
| 安装与体检 | `init-db`、`config`、`doctor`、`version` |
| 运行舰队 | `tick`、`loop`、`integrate`、`serve [--ipc]` |
| 与 agent 交互 | `submit`、`say`、`ack` |
| 查看 | `status`、`fleet`、`tail`、`alerts`、`alert`、`scorecard` |
| Mission | `mission`、`task`（含 `task commits`、`task diff`、`task land`）、`resident`、`objective`、`verify` |
| 分级与代码图 | `grade`、`features`、`prov` |
| 节点与存储 | `node`、`backup`、`store`、`source`、`scratch`、`commit-map`、`mask` |

节点发布：`scripts/fleet-build-manifest.sh` 打包 `spawn-node`（或完整 Spawn）构建并写出带 checksum 的 `release.manifest`；`scripts/fleet-sync-node.sh --manifest <file|url>` 把它拉到节点上：校验 checksum、替换二进制、重启 unit，健康检查失败时回滚。`spawn node list` 显示每个节点上报的版本与构建 SHA（`--json`：`node_version`、`build_sha`）。

## 开发

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # PostgreSQL 测试需要 SPAWN_TEST_DATABASE_URL
spawn features check --deps enforce
```

落地门禁对一棵树跑全部检查：`scripts/rsi/gate.sh $PWD/.git HEAD HEAD $PWD/rsi/scenarios`（构建、clippy、测试、`scripts/e2e.sh`、黑盒场景、web 构建）。

## 文档

| 文档 | 内容 |
| --- | --- |
| [docs/architecture.md](./docs/architecture.md)、[docs/design.md](./docs/design.md) | 架构与技术方案 |
| [docs/fleet-architecture.md](./docs/fleet-architecture.md) | 判定、控制、mission、落地、节点、告警（英文） |
| [docs/api.md](./docs/api.md) | 每个 HTTP 路由及其对应的 CLI 命令（英文） |
| [docs/config.md](./docs/config.md) | 引擎配置：模型槽固定与专用 Claude Code 目录（`SPAWN_CLAUDE_CONFIG_DIR`）（英文） |
| [docs/storage.md](./docs/storage.md)、[docs/d6-cas-v1.md](./docs/d6-cas-v1.md) | 存储、工作区后端、CAS 格式（英文） |
| [docs/agent-grading.md](./docs/agent-grading.md) | 任务分级与路由（英文） |
| [docs/feature-map.md](./docs/feature-map.md) | Feature 注册表与依赖检查（英文） |
| [docs/node-protocol-v1.md](./docs/node-protocol-v1.md)、[docs/spawn-ipc-v1.md](./docs/spawn-ipc-v1.md) | `spawn-node` 线协议与本地 IPC 契约（英文） |
| [docs/rsi.md](./docs/rsi.md)、[docs/upgrading.md](./docs/upgrading.md)、[docs/glossary.md](./docs/glossary.md) | 自我迭代循环、升级与共享词汇（后两者英文） |

## 许可证

Apache-2.0
