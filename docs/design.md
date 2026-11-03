# Spawn 技术方案与接口契约

结论：Spawn 用 5 个 crate 连接现成组件。`spawn-core` 不碰 IO，只定义类型、端口与编排逻辑；实现层分别是 PostgreSQL、HTTP 判定、tmux 与 LayerFS 子进程；`spawn-cli` 做装配。全部代码同步阻塞。本文给出 crate 边界、ADT、端口契约、SQL 对象与验收口径，代码按本文实现。

## 1. 模块边界

| crate | 职责 | 不允许 |
| --- | --- | --- |
| `spawn-core` | ADT、端口、事件归一、Bridge、Observer、内存实现、一致性套件 | 文件、网络、进程 IO；依赖驱动 |
| `spawn-postgres` | `EventLog` 与 `CommandQueue` 实现、`schema.sql` | 编排逻辑、投影规则 |
| `spawn-jev` | 判定 HTTP 客户端 | 持有会话状态 |
| `spawn-host` | tmux 与 `layerfs` CLI 适配 | 解释事件内容 |
| `spawn-cli` | 子命令与配置装配 | 业务规则 |

依赖单向：

```text
spawn-core
  ├─ spawn-postgres
  ├─ spawn-jev
  └─ spawn-host
        ↑ 只有 spawn-cli 依赖全部实现层
```

`spawn-core` 的 13 个模块：`bridge`、`command`、`contract`、`event`、`id`、`ingest`、`memory`、`observation`、`observe`、`port`、`screen`、`shell`、`time`。

## 2. 身份与 ID

事件流是映射的唯一来源。Bridge 每绑定一个原生对象就追加一条事件，恢复时折叠得到当前映射。

| ADT | 前缀 | 说明 |
| --- | --- | --- |
| `AgentId` | `agent_` | 长期逻辑身份 |
| `SessionId` | `session_` | 连续历史，跨引擎与终端存活 |
| `ExecutionId` | `exec_` | 一次计算，绑定一个终端与工作区 |
| `TerminalName` | `spawn-` | tmux 会话名，由 `ExecutionId` 派生 |
| `CommandId` | 数字 | 命令序号，PG 自增 |
| `Principal`、`IdempotencyKey`、`BranchName`、`CommitId` | 无 | 外部原文包装 |
| `WorkspaceId` | 无 | 必须形如 `<sandbox>:/<path>` |

校验规则：

| 规则 | 实现 |
| --- | --- |
| 前缀与字符集 | `label!` 宏统一校验：1–256 字节、无控制字符；带前缀的 ID 在前缀后只允许 `[A-Za-z0-9_-]+` |
| 工作区形态 | `check_workspace`：必须含 `:`，冒号前非空，路径以 `/` 开头 |
| 无前缀类型 | 两参形式转调三参形式，传入 `|_raw| Ok(())` |
| 失败处理 | 返回错误，不 panic |

时间统一 `bigint` 毫秒。`Timestamp(i64)` 提供 `plus` 与 `since`；`Clock` 是 trait，`SystemClock` 与 `ManualClock` 实现，测试注入固定时钟。

## 3. 事件协议

信封 `NewEvent { agent_id, session_id, execution_id, source, source_event_id, kind }`，写入返回 `EventRecord { position, session_seq, recorded_at, event }` 与 `Appended { record, inserted }`。

`Source`：Claude、Codex、Bridge、Layerfs、Observer、Gateway。

| 事件名 | 产生方 | 载荷 |
| --- | --- | --- |
| `session.native_started` | hook、Codex | engine、native_session_id、transcript_path |
| `session.native_ended` | hook、Codex | reason |
| `session.linked` | Bridge | parent_session_id |
| `message.received` | hook | 无 |
| `notification.received` | hook | notification_type |
| `tool.started` | hook、Codex | tool_name、tool_use_id |
| `tool.completed` | hook、Codex | tool_use_id、duration_ms |
| `tool.failed` | hook、Codex | tool_use_id、error |
| `turn.stopped` | hook、Codex | input_tokens、output_tokens |
| `turn.failed` | hook、Codex | error |
| `execution.started` | Bridge | engine、terminal、workspace、command_id |
| `execution.ended` | Bridge | cause、exit_code |
| `artifact.committed` | Bridge | reason、artifact |
| `command.accepted` | Gateway 事务 | command_id |
| `command.finished` | Worker | command_id、succeeded、error |
| `screen.observed` | Bridge | state、evidence |
| `observation.recorded` | Observer | observation |

`turn.stopped` 只表示模型本轮停止，不等于任务完成。任务完成只由 `SemanticState::Completed` 表达。

去重：`(session_id, source, source_event_id)` 唯一。Bridge 重放同一 hook 载荷返回既有记录，`inserted = false`。

## 4. 端口契约

`spawn-core::port` 定义四个 trait 与错误类型。

| trait | 方法 | 语义 |
| --- | --- | --- |
| `EventLog` | `append` | 追加一条事件，去重返回既有记录 |
| | `read_session(session, after, limit)` | `session_seq > after` 的会话事件 |
| | `read_global(after, limit)` | `position > after` 的全部事件，位置可见性单调 |
| `CommandQueue` | `submit(principal, submission)` | 幂等提交，同时写 `command.accepted` |
| | `claim(worker, ttl)` | 领取最老且无活跃租约的会话首条命令 |
| | `finish(claimed, outcome)` | 记录结果并写 `command.finished` |
| `WorkspaceStore` | `ensure_branch`、`mount`、`commit`、`unmount` | LayerFS 的四个动作；`commit` 返回 `Created(CommitId)` 或 `Unchanged` |
| `TerminalHost` | `start`、`submit_text`、`is_alive`、`capture_tail`、`terminate` | 终端生命周期与屏尾读取；`start` 之后每个方法都再传一次 `WorkspaceId`，实现层不在进程间保存状态 |

错误类型：

| 类型 | 取值 |
| --- | --- |
| `StoreError` | `LeaseLost{command,attempt}`、`Rejected`、`Corrupt`、`Backend` |
| `WorkspaceError` | `Refused{code,message}`、`Unavailable` |
| `TerminalError` | 单字段消息 |

并发规则：`append` 串行化；PG 实现用 `pg_advisory_xact_lock` 保证同库写入顺序，并用唯一索引兜底。

## 5. Agent Bridge

Bridge 做四件事：领取命令、启动与恢复执行、投递文本、提交工作区。

| 命令 | 行为 |
| --- | --- |
| `Start{engine,prompt,parent_session_id}` | 检查会话是否已有执行 → 开分支 → mount → 写 `execution.started` → 启动终端 → Claude 走屏尾等待投递。终端启动失败走结束流程，原因记为 `Lost` |
| `Input{text}` | 屏尾非 Ready 直接拒绝，Ready 才 `send-keys` |
| `Stop` | 走结束流程，原因记为 `Stopped` |
| `Resume{prompt}` | 复用最近的原生会话 id，新 Execution，逻辑 Session 不变 |

一次启动的参数由 `claude_arguments` 与 `codex_arguments` 生成：

| 引擎 | 参数 |
| --- | --- |
| Claude | `--ax-screen-reader --settings <内联 JSON> [--resume <id>]` |
| Codex | `exec --json [resume <id>] -- <prompt>` |

`--settings` 注入七类 hook：`SessionStart`、`UserPromptSubmit`、`PreToolUse`、`PostToolUse`、`Stop`、`SessionEnd`、`Notification`，命令统一是 `<spawn 绝对路径> hook`。钩子通过环境变量 `SPAWN_AGENT_ID`、`SPAWN_SESSION_ID`、`SPAWN_EXECUTION_ID`、`SPAWN_ENGINE` 定位会话。

启动参数超过 7 个时按 `Run { engine, native, prompt }` 结构体传引用，保持签名可读。

checkpoint 规则：工作区有改动才 commit，成功后写 `artifact.committed`，`source_event_id` 使用 CommitId；`Unchanged` 不产生事件。

结束流程 `end`：最后一次 checkpoint → `terminate` → `unmount` → 写 Bridge 来源的 `execution.ended`，键 `<exec>:ended`。`terminate` 失败时不写事件，执行保持活跃，下一次 tick 重试。

结束的三个来源：

| 来源 | 触发 | `cause` |
| --- | --- | --- |
| `Stop` 命令 | 用户 | `Stopped` |
| `spawn run` | 引擎退出，runner 写引擎来源的 `execution.ended`，键 `<exec>:exited` | `Exited`，带退出码 |
| tick 发现终端不在 | 进程崩溃或终端被外部杀掉 | `Lost` |

引擎来源的结束事件只是报告；Bridge 下一次 tick 读到后执行结束流程并写自己的结束事件。折叠时只有 Bridge 来源的 `execution.ended` 清除活跃执行。

恢复：每次 `spawn tick` 是新进程，从位置 0 折叠全部事件得到活跃执行、原生会话 id 与待提交的 checkpoint。`Resume` 复用最新原生会话 id，新开 Execution 与 mount，优先 `--resume`；逻辑 Session 不变。

## 6. 屏尾判定

Claude 以 `--ax-screen-reader` 运行，屏幕是唯一可靠的完成信号来源。hooks 给结构化事件；屏尾只判两件事：能否输入、是否被阻塞。

| 常量 | 值 | 含义 |
| --- | --- | --- |
| `TAIL_LINES` | 40 | 判定窗口行数 |
| `BODY_WINDOW` | 14 | 提示符上方正文窗口 |
| `PROMPT` | `❯` | 输入提示符 |
| spinner 集合 | `✻ ✽ ✳ ✢ ✶ ✷ ✸ ✹` | 运行中的旋转符 |
| `FINISHED` | `· done` | 本轮结束标记 |

`ScreenState`：`Ready`、`Drafted`、`Busy`、`Permission`、`Error`、`Unknown`。只有 `Ready` 允许输入。

解析顺序：

1. 去掉 ANSI 转义。
2. 丢弃尾部空白行，再取最后 40 行。空屏底部留白不得挤掉真实文本。
3. 取窗口内最低的 `❯` 行作为提示符行。
4. 提示符行带选项编号判 `Permission`；提示符后为空判 `Ready`，有文本判 `Drafted`。
5. 正文窗口内自下而上匹配权限行、运行中行、`esc to interrupt`、报错行。
6. 都不匹配判 `Unknown`。

严格尾匹配的含义：窗口外的旧 spinner、旧报错、旧提示符不参与判定。判定结果与决定它的原始行一起写入 `screen.observed`。

## 7. Observer 与判定

`Observation { state, reason, based_on_seq, confidence, judge }`。`SemanticState` 取值 `Working`、`Waiting`、`Blocked`、`Completed`、`Unknown`，序列化为 `SCREAMING_SNAKE_CASE`，`ALL` 数组即声明顺序。

`JudgeInput` 字段：`agent_id`、`session_id`、`engine`、`execution_active`、`last_end`、`open_tools`、`last_tool`、`permission_pending`、`screen`、`turn`、`last_error`、`evidence_seq`。

Observer 每次 tick：

1. 从游标读取增量，折叠成会话快照。
2. 调 `StateJudge`。
3. 状态或原因变化才追加 `observation.recorded`，`based_on_seq` 记录依据。
4. `source = Observer` 的事件不进入快照；`observation.recorded` 只恢复上一次结论与 `based_on_seq`，新进程不重复写同一结论。

快照规则：hook 事件（工具、消息、回合）比屏尾读数新，到达时清空 `screen` 与 `permission_pending`。

判定实现：

| 实现 | 用途 |
| --- | --- |
| `HeuristicJudge` | 纯规则，离线可用，测试基线。`name()` 返回 `heuristic` |
| `JevJudge` | HTTP，见第 8 节。`name()` 返回 `jev` |

判定错误只产生 `Unknown + JudgeUnavailable`，不标记 Agent 死亡。

## 8. Jev HTTP 契约

请求 `POST <base>/v1/systemone`，请求体：

```json
{"state": <任意 JSON>, "questions": {"semantic_state": {"type": "choice", "instructions": "...", "criteria": {"WORKING": "...", ...}}}}
```

`criteria` 必须编码为保序对象，键是 `SemanticState::ALL` 的枚举名。服务端把数组元素整体当选项名，二元组数组会被字符串化成错误键。

响应读取 `answers.semantic_state`：`choice` 缺失时回退概率最大项，此时 `confidence` 记为 `Confidence::ZERO`。

| 情况 | 处理 |
| --- | --- |
| 空 `answers` | `JudgeError::Unavailable` |
| 非 2xx 且带 body | 错误信息同时含状态码与 body |
| 连接失败 | `Unavailable` |
| `Transfer-Encoding: chunked` | 分块解码 |
| base 尾部有斜杠 | 规范化后再拼接 |

客户端由标准库 socket 手写 HTTP/1.1，不引入 HTTP 库。

## 9. PostgreSQL schema

表：

| 表 | 关键列 |
| --- | --- |
| `spawn_events` | `id`、`agent_id`、`session_id`、`execution_id`、`source`、`source_event_id`、`kind`、`session_seq`、`recorded_at_ms`、生成列 `event_name` |
| `spawn_commands` | `principal`、`idempotency_key`、`state`、`lease_until_ms`、`lease_attempt` |
| `spawn_grants` | 授权 principal |

函数：

| 函数 | 返回 | 说明 |
| --- | --- | --- |
| `spawn_now_ms()` | `bigint` | 单一时间源 |
| `spawn_append(...)` | `out_id`、`out_seq`、`out_at`、`out_inserted` | 事务内取序号并插入，`pg_advisory_xact_lock(1399957335)` 串行化 |
| `spawn_submit(...)` | `out_id` | 幂等插入并写 `command.accepted` |
| `spawn_claim(worker, ttl_ms)` | `out_id`、`out_attempt`、`out_agent`、`out_session`、`out_action` | `for update skip locked`，单会话串行，过期租约 attempt+1 |
| `spawn_finish(command, attempt, succeeded, error)` | `done`、`already`、`stale`、`missing` | 旧 attempt 结束被拒 |

权限：视图对 public 可读；`spawn_events`、`spawn_commands`、`spawn_grants` 撤销 public 全部权限；四个写函数撤销 public 执行权。`spawn_gateway_insert` 固定 `search_path = <schema>, pg_temp`，调用方无法用临时表替换 `spawn_grants`。

视图：`agent_state`（每会话最新 observation）、`agent_timeline`、`agent_tasks`、`agent_executions`、`agent_artifacts`、`agent_relations`、`agent_commands`。视图取代早期物化视图与 CDC 方案。

`agent_commands` 上挂 `instead of insert` 触发器 `spawn_gateway_insert`，`security definer`，查 `spawn_grants`，无授权 raise `42501`。

## 10. tmux 与 LayerFS 契约

tmux：

| 调用 | 要点 |
| --- | --- |
| `new-session -d -s <name> -x 200 -y 50 [-e K=V] -- prog …` | 每次执行一个会话 |
| `has-session`、`kill-session` | 会话 target：`=<name>` |
| `send-keys`、`capture-pane` | pane target：`=<name>:`，即当前窗口的活跃 pane；裸 `=<name>` 会被当作 pane 规格而报错，`:.0` 在 `pane-base-index 1` 下失效 |
| 重复会话 | stderr 为 `duplicate session: …`，适配层归一为 `already exists` |

LayerFS：一次调用对应一次 `@layerfs/agent` 调用。

| 退出码 | 含义 |
| --- | --- |
| 0 | 成功，stdout 是裸 JSON |
| 2 | 用法错误，未执行任何操作 |
| 125 | 拒绝或失败，stderr 是 `{code,message,detail}` |

| 调用 | 响应 | 映射 |
| --- | --- | --- |
| `sandbox create` | `{"sandbox":"9"}` | 沙箱编号 |
| `workspace mount <sb> <branch>` | `{"ws":"9:/workspaces/9"}` | `WorkspaceId` |
| `workspace commit … -m <msg>` | `{"commit":{"id":"c_9"}}` 或 `{"upToDate":true}` | `Created` 或 `Unchanged` |
| 无变更 | `NoCommit` 拒绝码 | `Unchanged` |

`exec_prefix` 为 `layerfs --project <p> workspace exec <ws> --stream --`，共 8 段；tmux 命令整体作为一段传入前缀程序。

## 11. 编译与磁盘策略

| 手段 | 配置 |
| --- | --- |
| 依赖最小化 | 无 async 运行时、无 chrono/uuid/rand、无 HTTP 客户端库 |
| 驱动隔离 | `postgres` 只在 `spawn-postgres`，HTTP 只在 `spawn-jev` |
| 工具链 | edition 2024，`rust-version` 1.95 |
| 调试信息 | dev `debug="line-tables-only"`、`split-debuginfo="packed"`；release `lto="thin"`、`codegen-units=1` |
| 编译缓存 | 远端门禁 `RUSTC_WRAPPER=kache`，`CARGO_INCREMENTAL=0` |
| 登录开销 | dev profile 对 `sha2`、`hmac`、`postgres-protocol` 开 `opt-level = 3`，SCRAM 登录从约 50 ms 降到约 6 ms |
| 目标目录 | 远端固定 `CARGO_TARGET_DIR`，kache 复用，避免多轮 rsync 占盘 |

## 12. 测试策略

只测公开 API：断言全部位于各 crate 的 `tests/api.rs`，库内不写 `#[cfg(test)]`。

| crate | 覆盖 |
| --- | --- |
| `spawn-core` | ID 校验、事件往返、去重、双序号、租约过期、屏尾六态、Bridge 事件序、每 tick 新进程折叠下的 Start→Stop→Start→Lost、Observer 跨进程不重复写 |
| `spawn-host` | LayerFS 三种退出码与全部 JSON 形态、tmux 精确名匹配、前缀注入与跨进程调用、真实 tmux 生命周期 |
| `spawn-jev` | 请求体结构与 criteria 顺序、真实响应往返、缺失字段回退、错误降级、chunked |
| `spawn-postgres` | schema 首次执行、幂等、稠密序号、单会话串行、租约重取、视图、授权拒绝、共享一致性套件 |

共享一致性套件 |

共享一致性套件在 `spawn-core::contract`：`event_log`、`command_queue`、`workspace_store`、`terminal` 四个函数，任一实现都必须通过。

门禁在 digest 执行：

| 步骤 | 命令 |
| --- | --- |
| 工具链 | `cargo --version`、`kache --version` |
| 数据库 | 临时容器 `postgres:16-alpine`，注入 `SPAWN_TEST_DATABASE_URL` |
| 格式 | `cargo fmt --all -- --check` |
| 静态检查 | `cargo clippy --workspace --all-targets -- -D warnings` |
| 测试 | `cargo test --workspace --no-fail-fast` |
| 端到端 | `scripts/e2e-smoke.sh`：真 PG、真 tmux、假 LayerFS、假 Claude 屏，断言 `--ax-screen-reader`、前缀调用、提示落到屏、observation 入库；按阶段打印 calls、total、max 与 submit 到回显的延迟 |

缺少 `SPAWN_TEST_DATABASE_URL` 时 PG 测试显式 panic，不静默跳过。

## 13. 已知限制

| 项 | 现状 | 升级路径 |
| --- | --- | --- |
| 无常驻进程 | 每次 `spawn tick` 连接 PG、从位置 0 折叠全部事件；耗时随事件数线性增长 | 常驻 worker 持有游标，或按会话快照 |
| 租约续期 | Bridge 不续租；单条命令超过 `lease_ttl`（2 分钟）会被别的 worker 重取 | tick 内续租，或缩短单命令时长 |
| 重试上限 | `spawn_claim` 无最大 attempt，坏命令会被反复领取 | 加 `max_attempts`，超限直接 `finish(failed)` |
| schema 版本 | `migrate` 每次整体执行 `schema.sql`，无版本表 | 编号迁移文件与版本表 |
| 子进程超时 | `layerfs`、`tmux` 调用没有超时 | `wait_timeout` 或独立线程计时 |
| 屏尾报错 | `Error` 状态靠关键字匹配，误判可能 | 由 hooks 的 `turn.failed` 取代 |
| 视图读取 | Rust 侧无读视图 API，`spawn status` 扫描原始事件 | 见 [view-architecture.md](./view-architecture.md) |
| 订阅 | 读取 `spawn_events` 增量；`LISTEN/NOTIFY` 未接入 | 接入 `LISTEN/NOTIFY` |
| 写入串行 | `spawn_append` 用一把全局 advisory lock | 按会话分锁 |
| 判定入参 | `JudgeInput` 的 serde 输出与真实服务入参是否语义等价未验证 | 用真实服务做契约测试 |
| LayerFS | `unmount` 会顺带 `sandbox close`；复用已有沙箱时可能误关 | 分开 unmount 与 close |
| tmux 输入 | `submit_text` 逐行发送，空串不发送任何内容 | `paste-buffer` |
| mount 泄漏 | mount 与 `execution.started` 之间崩溃会遗留一个 mount | 启动时清扫未记录的 mount |
