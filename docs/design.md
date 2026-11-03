# Spawn 技术方案与接口契约

结论：Spawn 不实现 Agent 推理框架，只用 Rust 实现三类连接器——Agent Bridge、事件存储适配、SQL Projector/Gateway，并通过 trait 端口隔离 tmux、LayerFS CLI、PostgreSQL、决策模型四类外部进程。本文定义 ADT、trait 边界、不变量和验收口径。代码按本文实现；与 README 冲突时以 README 的模块职责为准。

## 1. 范围与模块边界

五块能力对应十三个 crate。Compute 与 Observer 只依赖存储端口，互相不直接调用。

| 层 | crate | 职责 | 不允许做的事 |
| --- | --- | --- | --- |
| 领域 | `spawn-types` | 全部 ADT、校验、ID 生成、时钟端口 | 不出现 IO、不依赖运行时 |
| 进程 | `spawn-process` | 子进程执行端口与系统实现 | 不含业务语义 |
| 存储 | `spawn-store` | EventLog/CommandQueue 端口与内存实现 | 不解析引擎原生事件 |
| 存储 | `spawn-store-postgres` | PostgreSQL 端口实现与迁移 | 不包含投影规则 |
| 计算 | `spawn-engine` | Claude Hooks 与 Codex JSONL 归一化、启动参数 | 不直接写库、不起进程 |
| 计算 | `spawn-layerfs` | ArtifactStore 端口与 layerfs CLI 适配 | 不依赖 tmux、不写事件 |
| 计算 | `spawn-terminal` | TerminalHost 端口与 tmux 实现 | 不解释事件内容 |
| 计算 | `spawn-bridge` | Session 映射、Checkpoint、恢复、命令消费编排 | 不实现推理循环 |
| 观测 | `spawn-observer` | 1Hz 调度、快照折叠、状态判定端口、防递归 | 不直接调用具体模型 HTTP |
| 观测 | `spawn-judge-jev` | System One `/v1/systemone` 客户端 | 不持有 Session 状态 |
| 呈现 | `spawn-projector` | 事件到六个视图的纯折叠与游标 | 不接受消费者命令 |
| 呈现 | `spawn-gateway` | 授权校验、幂等命令提交、状态订阅 | 不直接执行 Agent 操作 |
| 装配 | `spawn-cli` | 子命令与配置装配 | 不放业务规则 |

依赖方向固定为单向。`spawn-types` 是叶子。端口 trait 与实现分 crate，重依赖（PostgreSQL 驱动、HTTP/TLS）只出现在适配 crate，核心 crate 编译不等待它们。

```text
spawn-types
  ├─ spawn-process
  ├─ spawn-store ── spawn-store-postgres
  ├─ spawn-engine
  ├─ spawn-layerfs
  ├─ spawn-terminal
  ├─ spawn-bridge ── engine, layerfs, terminal, store, process
  ├─ spawn-observer ── spawn-judge-jev
  ├─ spawn-projector
  └─ spawn-gateway
spawn-cli 只做装配，依赖全部端口与适配器
```

## 2. 统一身份与 ID 规则

逻辑 Session 独立于任何原生 ID。映射不靠独立表维护，而由事件流重建：Bridge 每绑定一个原生对象就追加一条绑定事件，恢复时折叠事件得到当前映射。

| ADT | 文本形态 | 生成方 | 说明 |
| --- | --- | --- | --- |
| `AgentId` | `agent_<22>` | Bridge | 长期逻辑身份 |
| `SessionId` | `session_<22>` | Bridge | 连续逻辑历史，跨引擎/终端存活 |
| `ExecutionId` | `exec_<22>` | Bridge | 一次计算，绑定一个终端与工作区 |
| `EventId` | `evt_<22>` | EventLog 写入侧 | 全局不可变事实标识 |
| `CommandId` | `cmd_<22>` | Gateway | 消费者命令 |
| `CommitId`/`BranchName`/`MountId`/`TerminalName` | 外部原文字符串 | LayerFS/tmux | 包装为 newtype，不假设格式 |

`<22>` 由毫秒时间戳、进程标识、进程内原子计数器拼接后 base32 编码得到。`IdGenerator` 是 trait，测试注入固定生成器。所有 newtype 校验非空、长度上限 256 字节、不含控制字符；校验失败返回 `ValidationError`，不 panic。

双序号口径：

| 序号 | 作用域 | 分配者 | 用途 |
| --- | --- | --- | --- |
| `SessionSeq` | 单 Session 内单调，从 1 开始 | EventLog 写入事务 | 会话内因果与 Observer 游标 |
| `GlobalPosition` | 全库单调 | EventLog 写入事务 | Projector/CDC 全量扫描，不依赖多系统时钟 |

## 3. 事件协议（核心 ADT）

### 3.1 信封

`NewEvent` 由生产方构造，`EventRecord` 由 EventLog 在成功写入时返回。外部来源事件带 `source_event_id`，写入侧用 `(session_id, source, source_event_id)` 唯一约束去重；Bridge 重放同一 Hooks 输入不产生第二条事实。

| 字段 | 类型 | 约束 |
| --- | --- | --- |
| `event_id` | `EventId` | 写入侧分配 |
| `agent_id`/`session_id`/`execution_id` | newtype | execution 允许空，表示 Session 级事件 |
| `session_seq`/`global_position` | `u64` | 写入侧分配 |
| `source` | `EventSource` | `ClaudeHooks`/`CodexStream`/`LayerFs`/`Bridge`/`Observer`/`Gateway`/`Runtime` |
| `source_event_id` | `Option<String>` | 外部去重键 |
| `kind` | `EventKind` | 见 3.2 |
| `artifact` | `Option<ArtifactRef>` | 文件类事件必填 |
| `occurred_at` | `Timestamp` | 毫秒 UTC；写入侧用注入 Clock 补全 |

### 3.2 EventKind 与外部事件映射

事件名使用点分层级，序列化后与 README 的统一事件名一致。Claude 与 Codex 的原生事件只在 `spawn-engine` 内映射，其余 crate 只认识统一事件。

| 统一 kind | Claude Hooks | Codex `exec --json` | 载荷要点 |
| --- | --- | --- | --- |
| `session.native_started` | `SessionStart` | `thread.started` | engine、native_session_id、transcript_path |
| `message.received` | `UserPromptSubmit` | turn 前用户输入 | prompt 摘要/引用 |
| `tool.started` | `PreToolUse` | `item.started` | tool_name、tool_use_id/item.id、参数引用 |
| `tool.completed` | `PostToolUse` | `item.completed` 成功 | 工具调用关联 id、duration_ms |
| `tool.failed` | `PostToolUseFailure` | `item.completed` 失败/`error` | 错误分类与原文 |
| `turn.stopped` | `Stop` | `turn.completed` | usage 计数 |
| `turn.failed` | `StopFailure` | `turn.failed` | 失败类型 |
| `session.native_ended` | `SessionEnd` | 线程结束/进程退出 | end_reason |
| `execution.started`/`execution.exited`/`execution.lost` | Bridge/Runtime 产生 | 同左 | 退出码或丢失原因 |
| `artifact.committed` | Bridge 在 Checkpoint 后产生 | 同左 | ArtifactRef、CheckpointReason |
| `command.accepted`/`command.claimed`/`command.finished` | Gateway/Queue 产生 | 同左 | 命令与租约 |
| `observation.recorded` | Observer 产生 | 同左 | Observation |
| `relation.linked` | Bridge 产生 | 同左 | parent_agent/parent_session |

`Stop` 只表示模型本轮停止，不等价任务完成，语义完成只由 Observation 的 `COMPLETED` 表达。

### 3.3 文件与事件的顺序和对账

Checkpoint 顺序固定：先在 LayerFS 持久化 Commit 成功，再追加 `artifact.committed`，其 `source_event_id` 直接使用 CommitId。事件写入失败时重试插入；唯一约束保证同一 Commit 只记一次。LayerFS 返回 `NoChanges` 时不产生 Commit、不产生事件。未确认 Commit 持久化前，事件载荷不得声称已持久化。

## 4. 存储端口

### 4.1 EventLog

```text
append(NewEvent) -> AppendOutcome { Recorded(EventRecord) | Deduplicated(EventRecord) }
read_session(session, since_seq, limit) -> Vec<EventRecord>
read_global(since_position, limit) -> Vec<EventRecord>
session_cursor() -> Vec<SessionCursor>
```

契约：`append` 是串行化的；内存实现用互斥锁，PostgreSQL 实现用单事务内更新会话序号表并插入事件，唯一索引兜底去重。读接口只返回已提交数据。

### 4.2 CommandQueue 与租约

| 方法 | 语义 |
| --- | --- |
| `submit(CommandRecord)` | 幂等键唯一；重复提交返回既有命令 |
| `claim(worker, now, ttl)` | 同一 Session 同时最多一条命令被领取；`SKIP LOCKED` 跳过他人租约 |
| `renew(token, now, ttl)` | 租约持有者才能续租，fencing token 不匹配报错 |
| `finish(token, outcome, now)` | 终态不可逆；过期租约可被其他 Worker 重新领取 |

`LeaseToken` 含命令 id 与单调 attempt 序号，作为 fencing token。Worker 重启后只认当前 attempt，旧 attempt 的外部写回被拒绝，避免重复执行外部操作。

## 5. Agent Bridge 行为

Bridge 只做四件事：启动恢复、事件采集、文件提交、身份映射。它持有三个端口引用：`EventLog`、`ArtifactStore`、`TerminalHost`。

| 操作 | 行为序列 |
| --- | --- |
| open_session | 确保 Branch 存在（已存在视为成功）→ 追加 Session 打开与分支绑定 |
| start_execution | 创建/复用 Sandbox → mount 工作区 → 按 Engine 构造启动参数 → tmux 启动 → 追加 execution.started 与终端绑定 |
| ingest_native | engine crate 归一化 → 追加统一事件；工具结束事件触发 Checkpoint 评估 |
| checkpoint | 工作区有改动才 commit → 成功后追加 artifact.committed；NoChanges 为空操作 |
| finish_turn | turn.stopped 后做最终 Checkpoint |
| recover | 折叠事件取最新 Commit 与原生 session id → 重新 mount → 优先原生 resume，失败则新开原生会话并加载上下文 → 创建新 Execution，逻辑 Session 不变 |

Checkpoint 触发规则：文件类工具成功结束、一轮结束、长任务定时巡检发现脏工作区。Bridge 不解析模型决策，不注入推理提示。

## 6. ArtifactStore 与外部进程端口

LayerFS 只发布 JavaScript SDK 与 `layerfs` 命令，因此 Rust 侧以子进程边界接入，不引入 Node 依赖。成功时 stdout 输出裸 JSON 对象；结构化拒绝走退出码 125 并携带 `{code,message,detail}`。适配层把协议错误映射为 `ArtifactError::Refused{code,message,detail}`，进程级错误映射为 `ArtifactError::Unavailable`。无更改提交的拒绝码为 `NoCommit`，适配层映射为 `CommitOutcome::NoChanges`；合并冲突拒绝码为 `Conflict`，冲突路径取自 `detail.conflicts`。

| 端口方法 | 对应 layerfs 调用 | 关键结果判定 |
| --- | --- | --- |
| `ensure_branch` | `branch fork` | `Forked` 或 `NameTaken` 均为成功 |
| `create_sandbox` | `sandbox create` | 返回 sandbox 编号 |
| `mount` | `workspace mount` | `{sandbox,path,image}` → MountId |
| `commit` | `workspace commit` | `Committed` 取 commit.id；`NoChanges` 为空 |
| `unmount` | `workspace unmount` | 支持 discard |
| `list_commits` | `branch list_commits` | 分页 items/hasMore |
| `merge` | `branch merge` | `Merged` 或结构化 `Conflict` |

`spawn-process` 定义 `CommandLine` 与 `ProcessRunner`，测试用脚本假机替换可执行文件，验证参数顺序、stdout JSON 解析与退出码映射，不依赖真实 LayerFS。

TerminalHost 端口方法：`start(LaunchSpec)`、`send_keys`、`capture_pane`、`is_alive`、`terminate`。tmux 实现只通过 `tmux` 命令操作；在 Sandbox 内运行时由 `LaunchSpec.prefix` 注入容器 exec 前缀，端口本身不感知容器。

## 7. Observer 与状态判定

执行事实与语义状态分离。进程退出、工具完成、心跳丢失是事实事件；WORKING/WAITING/BLOCKED/COMPLETED/UNKNOWN 是判定结果。

| ADT | 取值 |
| --- | --- |
| `SemanticState` | `Working`/`Waiting`/`Blocked`/`Completed`/`Unknown` |
| `WaitReason` | `AwaitingUserPermission`/`AwaitingUserInput`/`IdleTurnStop`/`JudgeUnavailable`/`ObservationExpired`/`Other(String)` |
| `Confidence` | `f64`，构造校验闭区间 `[0,1]` |

Observer 每次 tick：

1. 读取每个活跃 Session 自上次游标后的增量，折叠出 `SessionSnapshot`（开放工具、最后活动时间、最后事实）。
2. 增量为空也重新评估时间条件（心跳超时），但默认不重复写相同结论。
3. 调 `StateJudge` 端口；判定错误或超时只产生 `Unknown + JudgeUnavailable`，不标记 Agent 死亡。
4. 状态或原因变化、或首次判定时追加 `observation.recorded`，`based_on_seq` 记录判定依据。
5. 读取增量时跳过 `source=Observer` 事件，Observation 不递归触发新 Observation。

判定端口两个实现：`HeuristicJudge`（纯规则，离线可用，也是测试基线）和 `JevJudge`（HTTP）。JevJudge 一次请求提交 choice/score/noul 三类问题，choice 枚举固定为五个 SemanticState，noul 用于判断是否阻塞，score 用于置信校准；端点、模型名、超时、可选 Bearer token 全部配置注入。System One 契约与 v100 自部署 StartLux 服务共用同一套请求响应结构。choice 的 `criteria` 必须序列化为 JSON 对象 `{选项键: 描述}`，选项键即五个状态枚举名；服务端把数组元素整体当作选项名，二元组数组会被字符串化成错误键，因此客户端用保序 map 编码，不能发 `[键, 描述]` 数组。响应 `answers.semantic_state.choice` 回传裸枚举键，客户端据此映射 `SemanticState`。

## 8. Projector 与 Gateway

Projector 维护 GlobalPosition 游标，按批读取事件并纯折叠成六类视图行：`agent_state`、`agent_timeline`、`agent_tasks`、`agent_artifacts`、`agent_executions`、`agent_relations`。折叠函数无 IO、可重入：重启后从上一提交游标继续，幂等 upsert 不产生重复行。文件视图的 Commit 元数据只经 ArtifactStore 后端读取能力获取，不读取 LayerFS 私有 SQLite。

Gateway 是唯一命令写入口：

| 步骤 | 规则 |
| --- | --- |
| 校验 | Principal 授权 + CommandAction 字段校验 + 载荷上限 |
| 幂等 | 同一 Principal 的 IdempotencyKey 命中则返回原命令 |
| 写入 | 校验通过才写命令表并追加 command.accepted |
| 订阅 | 只读视图 + 版本号变化通知；订阅本身不产生事件 |

查询面同时暴露两条等价路径：Rust `ViewStore` 方法，以及 PostgreSQL 内只读 SQL 视图。Gateway 不实现 SQL 解析器，不接受任意 DDL/DML。

## 9. 编译耗时与磁盘策略

| 手段 | 配置 |
| --- | --- |
| 叶子拆分 | 纯 ADT crate 零重依赖；postgres/HTTP 隔离在独立 crate |
| 依赖最小化 | 无 CLI 参数框架、无 async 运行时扩散、无时间/UUID 额外库；HTTP 仅 jev crate 使用 ureq+rustls |
| 工具链 | `rust-toolchain.toml` 固定 1.95.0，digest 已预装，避免远程下载 |
| 调试信息 | dev/test `debug = "line-tables-only"`，split debuginfo，链接更快体积更小 |
| 编译缓存 | 远端门禁 `RUSTC_WRAPPER=kache`，关闭 incremental（wrapper 缓存不兼容 incremental 产物） |
| 目标目录 | 远端固定 `CARGO_TARGET_DIR=/data00/home/chenkailun.c/spawn-target`，kache hardlink 复用，多轮 rsync 不重复占盘 |
| 测试二进制 | 每 crate 单 tests 入口模块，减少重复链接 |

## 10. 测试策略

只测公开 API：每个 crate 的断言全部位于 `tests/` 集成测试，库内不写 `#[cfg(test)]` 私有单测。测试只经公开 trait/函数/ADT 构造行为。

| crate | 公开行为覆盖 |
| --- | --- |
| spawn-types | ID 校验、序列化往返、置信度/序号边界 |
| spawn-store | 去重、双序号、租约领取/过期/fencing、重启语义 |
| spawn-engine | 两类原生样例到统一事件的完整映射与未知事件拒绝 |
| spawn-layerfs | 假 layerfs 可执行文件验证参数、JSON、退出码 125 映射 |
| spawn-terminal | digest 真机 tmux 会话生命周期 |
| spawn-bridge | 内存端口端到端：Checkpoint 顺序、NoChanges、恢复映射、重试对账 |
| spawn-observer | 1Hz tick、状态变化才写入、防递归、超时 Unknown、游标恢复 |
| spawn-judge-jev | 本地 HTTP 假机验证请求结构与三类响应解析、错误降级 |
| spawn-projector | 六视图折叠、断点续跑幂等 |
| spawn-gateway | 授权拒绝、幂等返回、命令生命周期 |
| spawn-store-postgres | docker PostgreSQL：迁移、唯一去重、SKIP LOCKED 并发领取 |

PostgreSQL 测试由门禁脚本启动 `postgres:16-alpine` 并注入 `SPAWN_TEST_DATABASE_URL`；未注入环境时测试显式失败并打印指引，不允许静默 skip。门禁在 digest 执行，网络经代理，crate 下载走已配置的 rsproxy 镜像。

## 11. 待验证项

| 项 | 处置 |
| --- | --- |
| v100 StartLux 9B 在 V100(SM70) 上的引擎路线 | 已落地：llama.cpp SM70 构建 + Q8_0 GGUF（9.53GB），前端 `startlux_decide` 暴露 `/v1/systemone` 于 8090，`SPAWN_DECISION_URL` 填根地址；验收通过、中位延迟约 0.29s |
| LayerFS 真实 Sandbox 与 FUSE 可用性 | 本期用协议假机保证适配逻辑；真机联调在 M1 阶段接入 |
| Materialize SUBSCRIBE | V1 用 PG 视图 + 版本通知满足订阅语义，Materialize 作为后续 CDC 对接项 |
