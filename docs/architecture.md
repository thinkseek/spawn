# Spawn 架构总览

[English README](../README.md) · [中文 README](../README.zh-CN.md) · [技术方案与接口契约](./design.md) · [视图层设计](./view-architecture.md) · [舰队架构](./fleet-architecture.md) · [存储](./storage.md) · [RSI](./rsi.md) · [升级](./upgrading.md)

结论：Spawn 用 5 个 Rust crate 连接现成组件，不实现 Agent 推理框架。Claude Code 提供计算，git 提供工作区，PostgreSQL 保存事件并对外提供 SQL 视图，Jev 判定语义状态，`spawn` 二进制只做装配。全部代码同步阻塞，不使用 async。

## 1. 能力与边界

| 能力 | 实现方 | Spawn 做的事 |
| --- | --- | --- |
| 计算 | Claude Code | 启动进程、投递提示、读取 transcript、屏幕与退出文件 |
| 工作区 | git | 裸仓库加每分支一份工作树，`git archive` 导出、`commit-tree` 收口 |
| 事件存储 | PostgreSQL 16 | 建表、写入函数、视图、命令队列 |
| 语义状态 | 规则，加判定模型 `/v1/systemone`（未配置时只用规则） | 规则先判；规则定不了的情形发一次 HTTP 请求；写一条 observation |
| 对外接口 | SQL | 视图查询状态，`spawn_submit` 下发命令 |
| 终端 | tmux | 建会话、输入文本、抓取屏尾 |

Spawn 不实现：Agent 框架、提示工程、文件系统、状态机引擎。`spawn serve` 只提供只读看板和事件查询（默认绑 127.0.0.1，非回环地址必须设 `SPAWN_SERVE_TOKEN`），不是控制面；所有写入仍只经 Bridge 和 `spawn_submit`。

## 2. crate 结构

| crate | 职责 | 依赖 |
| --- | --- | --- |
| `spawn-core` | 类型、端口、事件归一、Bridge、Observer；内存实现与一致性套件在 `testing` feature 后。零 IO | serde、serde_json、thiserror |
| `spawn-postgres` | EventLog + CommandQueue 的 PG 实现、`schema.sql`、读方法 | spawn-core、postgres、sha2 |
| `spawn-jev` | 判定 HTTP 客户端，端口在 core，实现在此 | spawn-core、serde_json |
| `spawn-host` | tmux、git 工作区与块存储适配 | spawn-core、libc、blake3、zstd |
| `spawn-cli` | 二进制 `spawn`，装配上述实现 | 全部 |

```mermaid
flowchart LR
  core[spawn-core<br/>types · ports · bridge · observer]
  pg[spawn-postgres]
  jev[spawn-jev]
  host[spawn-host]
  cli[spawn-cli]
  pg --> core
  jev --> core
  host --> core
  cli --> pg
  cli --> jev
  cli --> host
```

依赖单向：只有 `spawn-cli` 同时依赖实现层，其余 crate 之间不互相依赖。

## 3. 一次任务的链路

```text
spawn_submit(principal, key, agent, session, action)
  → command.accepted          PG 事务，幂等
  → spawn_claim(worker, ttl)  单会话串行，租约 fencing
  → git branch / archive 导出工作树
  → tmux new-session          运行 spawn run -- claude --ax-screen-reader --session-id <uuid>
  → 屏尾判定 Ready            capture-pane 尾行解析
  → tmux send-keys            投递提示
  → Claude transcript         Bridge 每轮读 <projects>/<工作区>/<uuid>.jsonl，claude_line 归一为统一事件
  → 屏幕                      Bridge 每轮抓屏，两次读数一致才写 screen.observed
  → 退出文件                  spawn run 把退出码写入 .spawn/exit-<execution>，Bridge 读到后写 execution.ended
  → spawn_append              写 spawn_events，双序号
  → Observer tick             规则或模型判定语义状态
  → observation.recorded
  → agent_state / agent_timeline 视图
```

两条链路互不依赖：数据链路是引擎到事件到 SQL，状态链路是事件到判定再回到事件。

worker 的终端不持有数据库登录，引擎也不向 Spawn 汇报：会话 id 由 Bridge 用 `--session-id` 指定（由执行 id 派生的 UUID），因此引擎启动前 transcript 路径就已确定。

## 4. 终端接入：尾行解析

Claude 以 `--ax-screen-reader` 运行，屏幕文本是唯一可靠的完成信号来源。transcript 提供结构化事件，屏尾只回答两个问题：现在能否输入，是否被阻塞。

| 判定项 | 规则 |
| --- | --- |
| 窗口 | 先丢弃尾部空白行，再取最后 40 行；空屏的底部留白不参与判定 |
| 提示符 | 取窗口内最低的一个 `❯` 行，行内提示符后的文本决定颜色 |
| 状态 | Ready、Drafted、Busy、Permission、Error、Unknown |
| 可输入 | 只有 Ready 允许投递 |
| 证据 | 每个状态附带决定它的原始行，写入 `screen.observed` |
| 记录 | Bridge 每轮读一次屏；连续两次读数一致才记录，状态变化时记一条，Busy 持续时每 30 秒再记一条 |

必须做严格尾匹配：窗口外的旧 spinner、旧报错、旧提示符都不影响判定。

## 5. 事件与存储

`spawn_events` 是唯一事实表。字段：`agent_id`、`session_id`、`execution_id`、`source`、`source_event_id`、`kind`、`session_seq`、`recorded_at_ms`。

| 序号 | 作用域 | 用途 |
| --- | --- | --- |
| `session_seq` | 单会话，从 1 稠密递增 | 会话内因果、Observer 游标 |
| 事件 `id` | 全局 | 全局增量读取 |

去重键 `(session_id, source, source_event_id)`：Bridge 重读同一行 transcript 不产生第二条事实（键是引擎给的行 `uuid` 或 `tool_use_id`）。

事件名与载荷见 [design.md 第 3 节](./design.md)。

## 6. SQL 接口

| 对象 | 用途 |
| --- | --- |
| `spawn_append(...)` | 追加事件，返回 id、seq、时间、是否插入 |
| `spawn_submit(...)` | 幂等提交命令并写 `command.accepted` |
| `spawn_claim(worker, ttl_ms)` | 领取命令，单会话串行，过期租约 attempt+1 |
| `spawn_finish(...)` | 结束命令，返回 `done`、`already`、`stale`、`missing` |
| `agent_state` | 每会话最新 observation |
| `agent_timeline` | 会话事件时间线 |
| `agent_tasks`、`agent_executions`、`agent_artifacts`、`agent_relations` | 任务、执行、产物、父子关系 |
| `agent_commands` | 命令视图；`insert` 经 `spawn_gateway_insert` 校验 `spawn_grants` 后入队 |

视图不用物化视图和 CDC。订阅方式是读取 `spawn_events` 增量，没有 `LISTEN/NOTIFY`。

## 7. 用法

```bash
export SPAWN_DATABASE_URL=postgres://postgres:PASSWORD@127.0.0.1:15432/spawn
export SPAWN_GIT_REPO=/data/spawn/repo.git
export SPAWN_WORKSPACE_ROOT=/data/spawn/ws

spawn init-db
spawn submit --agent agent_001 --session session_001 --key k1 \
  --action '{"type":"start","engine":"claude","prompt":"整理最近三个提交"}'
spawn tick
spawn fleet
```

| 子命令 | 作用 |
| --- | --- |
| `init-db` | 安装或刷新 schema，可重复执行 |
| `submit` | 写一条命令，幂等键重复则返回原命令 |
| `tick` | 跑一轮 Bridge 与 Observer |
| `run` | 在终端内启动引擎，退出时把退出码写入 `SPAWN_EXIT_FILE`（Bridge 读到后写 `execution.ended{Exited}`），转发退出码；不连数据库 |
| `loop` | 常驻：每 500 ms 跑一轮 Bridge、Observer、派发与存活规则（`SPAWN_LOOP_INTERVAL_MS`） |
| `integrate` | 常驻：对已验收任务跑黑盒门禁，宿主机提交并以 CAS 前移来源基线 |
| `serve` | 常驻：只读看板与 HTTP 查询 |
| `mission` / `task` / `resident` | 任务域控制流：添加、配置、关闭、恢复任务，分派与验收子任务，提议、批准、退役常驻 worker |
| `fleet` / `tail` | 舰队概览（每个会话及其判定）、单个会话的判定与事件尾随 |
| `version` | 打印 `spawn <版本>` |

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PG 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 不属于任何任务的 agent（supervisor、临时 worker）用的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 每个分支一份工作树的根目录 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude 可执行文件 |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`，否则 `~/.claude/projects` | Claude 写 transcript 的目录 |
| `SPAWN_MAX_FILE_MB` | `100` | checkpoint 收录的单个文件上限（MiB）；更大的文件不进 checkpoint，并告知 agent 一次 |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux 可执行文件 |
| `SPAWN_TMUX_SOCKET` | 无 | 私有 tmux socket |
| `SPAWN_JUDGE_URL` | 未设 | 判定服务根地址，未设时用内置规则 |
| `SPAWN_WORKER` | `spawn-worker` | 租约中的 worker 名 |
| `SPAWN_LOOP_INTERVAL_MS` | `500` | `spawn loop` 两轮之间的间隔 |
| `SPAWN_GATE` | `scripts/rsi/gate.sh` | `spawn integrate` 的门禁脚本，输出一行 JSON 判定；线上指向控制器（部署的提交树）`~/.local/share/spawn-rsi/controllers/spawn/scripts/rsi/gate.sh`，从不指向所有者的检出：所有者的改动部署后才生效 |

## 8. 验证

```bash
scripts/rsi/gate.sh "$PWD/.git" HEAD HEAD "$PWD/rsi/scenarios"   # 唯一门禁：构建、clippy、测试、e2e、黑盒、web
scripts/rsi/test-gate.sh                                          # 门禁自身的测试
```

检查项都是 `rsi/scenarios/check-*.sh` 场景，见 [fleet-architecture.md](./fleet-architecture.md) §10.2。

## 9. 依赖约束

不使用 async 运行时，不使用 chrono、uuid、rand，不使用 HTTP 客户端库。时间统一为 `i64` 毫秒。`spawn-jev` 的 HTTP/1.1 客户端由标准库 socket 手写，支持 `Transfer-Encoding: chunked`。
