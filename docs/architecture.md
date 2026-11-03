# Spawn 架构总览

[English README](../README.md) · [中文 README](../README.zh-CN.md) · [技术方案与接口契约](./design.md) · [视图层设计](./view-architecture.md)

结论：Spawn 用 5 个 Rust crate 连接现成组件，不实现 Agent 推理框架。Claude Code 提供计算，LayerFS 提供工作区，PostgreSQL 保存事件并对外提供 SQL 视图，Jev 判定语义状态，`spawn` 二进制只做装配。全部代码同步阻塞，不使用 async。

## 1. 能力与边界

| 能力 | 实现方 | Spawn 做的事 |
| --- | --- | --- |
| 计算 | Claude Code、Codex CLI | 启动进程、投递提示、采集 hooks |
| 工作区 | LayerFS | 调用 `layerfs` CLI 完成 fork、mount、commit、unmount |
| 事件存储 | PostgreSQL 16 | 建表、写入函数、视图、命令队列 |
| 语义状态 | 判定模型 `/v1/systemone` 或内置规则 | 发一次 HTTP 请求，写一条 observation |
| 对外接口 | SQL | 视图查询状态，`spawn_submit` 下发命令 |
| 终端 | tmux | 建会话、输入文本、抓取屏尾 |

Spawn 不实现：Agent 框架、提示工程、文件系统、状态机引擎、调度器、Web 服务。

## 2. crate 结构

| crate | 职责 | 依赖 |
| --- | --- | --- |
| `spawn-core` | 类型、端口、事件归一、Bridge、Observer、内存实现、一致性套件。零 IO | serde、serde_json、thiserror |
| `spawn-postgres` | EventLog + CommandQueue 的 PG 实现、`schema.sql` | spawn-core、postgres |
| `spawn-jev` | 判定 HTTP 客户端，端口在 core，实现在此 | spawn-core、serde_json |
| `spawn-host` | tmux 与 `layerfs` 子进程适配 | spawn-core、serde_json |
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
  → layerfs branch fork / workspace mount
  → tmux new-session          运行 spawn run -- claude --ax-screen-reader
  → 屏尾判定 Ready            capture-pane 尾行解析
  → tmux send-keys            投递提示
  → Claude hooks              spawn hook 归一为统一事件
  → spawn_append              写 spawn_events，双序号
  → Observer tick             规则或模型判定语义状态
  → observation.recorded
  → agent_state / agent_timeline 视图
```

两条链路互不依赖：数据链路是引擎到事件到 SQL，状态链路是事件到判定再回到事件。

## 4. 终端接入：尾行解析

Claude 以 `--ax-screen-reader` 运行，屏幕文本是唯一可靠的完成信号来源。hooks 提供结构化事件，屏尾只回答两个问题：现在能否输入，是否被阻塞。

| 判定项 | 规则 |
| --- | --- |
| 窗口 | 先丢弃尾部空白行，再取最后 40 行；空屏的底部留白不参与判定 |
| 提示符 | 取窗口内最低的一个 `❯` 行，行内提示符后的文本决定颜色 |
| 状态 | Ready、Drafted、Busy、Permission、Error、Unknown |
| 可输入 | 只有 Ready 允许投递 |
| 证据 | 每个状态附带决定它的原始行，写入 `screen.observed` |

必须做严格尾匹配：窗口外的旧 spinner、旧报错、旧提示符都不影响判定。

## 5. 事件与存储

`spawn_events` 是唯一事实表。字段：`agent_id`、`session_id`、`execution_id`、`source`、`source_event_id`、`kind`、`session_seq`、`recorded_at_ms`。

| 序号 | 作用域 | 用途 |
| --- | --- | --- |
| `session_seq` | 单会话，从 1 稠密递增 | 会话内因果、Observer 游标 |
| 事件 `id` | 全局 | 全局增量读取 |

去重键 `(session_id, source, source_event_id)`：Bridge 重放同一 hook 输入不产生第二条事实。

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

视图取代早期方案的物化视图和 CDC。当前订阅方式是读取 `spawn_events` 增量；`LISTEN/NOTIFY` 尚未接入。

## 7. 用法

```bash
export SPAWN_DATABASE_URL=postgres://postgres:spawn@127.0.0.1:15432/spawn
export SPAWN_PROJECT=shop

spawn init-db
spawn submit --agent agent_001 --session session_001 --key k1 \
  --action '{"type":"start","engine":"claude","prompt":"整理最近三个提交"}'
spawn tick --project shop
spawn status
```

| 子命令 | 作用 |
| --- | --- |
| `init-db` | 安装或刷新 schema，可重复执行 |
| `submit` | 写一条命令，幂等键重复则返回原命令 |
| `tick` | 跑一轮 Bridge 与 Observer |
| `status` | 扫描全部事件，打印每会话最新一条 observation |
| `hook` | 从标准输入接收一条 Claude hook 载荷 |
| `run` | 在终端内启动引擎，退出时写 `execution.ended{Exited}`，转发退出码 |

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PG 连接串 |
| `SPAWN_PROJECT` | 无 | LayerFS 项目名 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude 可执行文件 |
| `SPAWN_CODEX_PROGRAM` | `codex` | Codex 可执行文件 |
| `SPAWN_LAYERFS_PROGRAM` | `layerfs` | LayerFS CLI |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux 可执行文件 |
| `SPAWN_TMUX_SOCKET` | 无 | 私有 tmux socket |
| `SPAWN_TMUX_IN_WORKSPACE` | 未设 | 设置后 tmux 经 LayerFS 前缀在沙箱内运行 |
| `SPAWN_JUDGE_URL` | 未设 | 判定服务根地址，未设时用内置规则 |
| `SPAWN_WORKER` | `spawn-worker` | 租约中的 worker 名 |
| `SPAWN_TRACE` | 未设 | 设置后每次端口调用向 stderr 打印一行 `spawn-trace <stage> <us>` |

## 8. 验证

```bash
scripts/remote-gate.sh     # 同步到 digest 并跑门禁
scripts/e2e-smoke.sh       # 真 PG + 真 tmux + 假 LayerFS + 假 Claude 屏
```

门禁顺序：工具链、临时 PG 容器、`cargo fmt --check`、`cargo clippy -D warnings`、`cargo test --workspace`、端到端冒烟。冒烟脚本按阶段计时，输出 calls、total、max 三列，并单列 `submit` 到屏上回显的延迟。

## 9. 依赖约束

不使用 async 运行时，不使用 chrono、uuid、rand，不使用 HTTP 客户端库。时间统一为 `i64` 毫秒。`spawn-jev` 的 HTTP/1.1 客户端由标准库 socket 手写，支持 `Transfer-Encoding: chunked`。
