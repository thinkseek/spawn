# Spawn 技术方案与接口契约

结论：Spawn 用 5 个 crate 连接现成组件。`spawn-core` 不碰 IO，只定义类型、端口与编排逻辑；实现层分别是 PostgreSQL、HTTP 判定、tmux 与 git 子进程；`spawn-cli` 做装配。全部代码同步阻塞。本文描述 crate 边界、ADT、端口契约、SQL 对象与验收口径。

## 1. 模块边界

| crate | 职责 | 不允许 |
| --- | --- | --- |
| `spawn-core` | ADT、端口、事件归一、Bridge、Observer、内存实现、一致性套件 | 文件、网络、进程 IO；依赖驱动 |
| `spawn-postgres` | `EventLog` 与 `CommandQueue` 实现、`schema.sql` | 编排逻辑、投影规则 |
| `spawn-jev` | 判定 HTTP 客户端 | 持有会话状态 |
| `spawn-host` | tmux、git 工作区与块存储适配 | 解释事件内容 |
| `spawn-cli` | 子命令与配置装配 | 业务规则 |

依赖单向：

```text
spawn-core
  ├─ spawn-postgres
  ├─ spawn-jev
  └─ spawn-host
        ↑ 只有 spawn-cli 依赖全部实现层
```

`spawn-core` 的 14 个模块：`bridge`、`command`、`contract`、`event`、`id`、`ingest`、`memory`、`mission`、`observation`、`observe`、`port`、`screen`、`shell`、`time`。

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

`Source`：Claude、Codex、Bridge、Observer、Gateway、Control。

| 事件名 | 产生方 | 载荷 |
| --- | --- | --- |
| `session.native_started` | Bridge（新会话由它命名）、Codex（`thread.started`） | engine、native_session_id、transcript_path |
| `session.native_ended` | 无；保留它以读取日志里已有的行 | reason |
| `session.linked` | Bridge | parent_session_id |
| `message.received` | 会话记录（transcript） | 无 |
| `notice.typed` | Bridge（键入的提示以合并通知开头时） | through（通知覆盖到的最后一个合并事件的位置） |
| `notice.received` | 会话记录（以合并通知固定开头的提示行） | 无 |
| `notification.received` | 无；保留它以读取日志里已有的行 | notification_type |
| `tool.started` | 会话记录、Codex | tool_name、tool_use_id |
| `tool.completed` | 会话记录、Codex | tool_use_id、duration_ms |
| `tool.failed` | 会话记录、Codex | tool_use_id、error |
| `turn.stopped` | 会话记录（`turn_duration`、中断）、Codex | input_tokens、output_tokens |
| `turn.failed` | 会话记录（API 错误行）、Codex | error |
| `execution.started` | Bridge | engine、terminal、workspace、command_id；本次启动新建了分支时另有 fork（from：`main`、启动的 `from` 或 `spawn/<agent>`；commit：分支起点） |
| `execution.ended` | Bridge | cause、exit_code |
| `execution.end_stalled` | Bridge（最后一次 checkpoint 连续失败 20 次，每个执行一次） | cause、exit_code、failures、error |
| `artifact.committed` | Bridge | reason（`tool_completed`、`turn_stopped`、`execution_ended`、`base_merged`）、artifact；`base_merged` 另带 files |
| `merge.conflicted` | Bridge | files、marked |
| `command.accepted` | Gateway 事务 | command_id |
| `command.finished` | Worker | command_id、succeeded、error |
| `screen.observed` | Bridge | state、evidence |
| `observation.recorded` | Observer | observation |
| `control.recorded` | 控制流（任务函数） | record（`{type, ...}`，见 fleet-architecture.md §10.1） |

`turn.stopped` 只表示模型本轮停止，不等于任务完成。任务完成只由 `SemanticState::Completed` 表达。

去重：`(session_id, source, source_event_id)` 唯一。Bridge 重读同一会话记录行返回既有记录，`inserted = false`。

## 4. 端口契约

`spawn-core::port` 定义五个 trait 与错误类型。

| trait | 方法 | 语义 |
| --- | --- | --- |
| `EventLog` | `append` | 追加一条事件，去重返回既有记录 |
| | `read_global(after, limit)` | `position > after` 的全部事件，位置可见性单调 |
| `CommandQueue` | `submit(principal, submission)` | 幂等提交，同时写 `command.accepted` |
| | `claim(worker, ttl)` | 领取最老且无活跃租约的会话首条命令 |
| | `finish(claimed, outcome)` | 记录结果并写 `command.finished` |
| `WorkspaceStore` | `ensure_branch(branch, from)`、`mount`、`commit`、`unmount` | 工作区的四个动作；`ensure_branch` 返回 `Existing` 或 `Created(Fork)`，新分支建在 `from`（另一个 agent 的分支头或完整 commit id，`ForkFrom::parse` 校验）或基线，已有分支忽略 `from`；`commit` 返回 `Created(CommitId)` 或 `Unchanged` |
| | `merge_base(workspace)` | 把基线（源的 `main`）合进空闲工作区的分支：`Current`、`Merged{commit,files}` 或 `Conflicted{files,marked}`；内容冲突把带冲突标记的合并写进目录（`marked`），等标记解决后的 checkpoint 提交合并；目录与分支不符、或标记表达不了的冲突什么都不改。默认实现返回 `Current`（内存实现可配置结果） |
| `ChunkStore` | `put`、`get`、`contains` | 按内容寻址的大文件对象存储：对象以其字节的哈希命名，写一次、不改，读时校验；`put` 遇到已有对象先校验，损坏的原子替换（修复，不是修改）（见 [storage.md](./storage.md)） |
| `TerminalHost` | `start`、`submit_text`、`send_keys`、`is_alive`、`capture_tail`、`read_from`、`terminate` | 终端生命周期、屏尾读取与读取引擎写的文件；`start` 之后每个方法都再传一次 `WorkspaceId`，实现层不在进程间保存状态 |

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
| `Start{engine,prompt,parent_session_id,from,model}` | 检查会话是否已有执行 → 开分支（`ensure_branch(branch, from)`；分支已存在时 `from` 被忽略并在 tick 报告里警告，`from` 解析不到 commit 时命令失败）→ mount → 写 `execution.started`（新分支带 fork）→ 启动终端 → Claude 走屏尾等待投递。终端启动失败走结束流程，原因记为 `Lost` |
| `Input{text}` | 屏尾非 Ready 直接拒绝，Ready 才 `send-keys` |
| `Stop` | 走结束流程，原因记为 `Stopped`；最后一次 checkpoint 失败时工作区不卸载 |
| `Discard` | 同 `Stop`，但最后一次 checkpoint 失败也卸载工作区，未提交的内容丢失；只由人或 supervisor 在 `execution.end_stalled` 之后发出 |
| `Resume{prompt,model}` | 复用最近的原生会话 id，新 Execution，逻辑 Session 不变 |

一次启动的参数由 `claude_arguments` 与 `codex_arguments` 生成：

| 引擎 | 参数 |
| --- | --- |
| Claude | `--ax-screen-reader --allow-dangerously-skip-permissions --settings <内联 JSON> (--session-id <uuid> \| --resume <uuid>)` |
| Codex | `exec --json [resume <id>] -- <prompt>` |

引擎模型（[fleet-architecture.md §3.3](fleet-architecture.md#33-engine-models)）：`Start`/`Resume` 的可选 `model` 是模型档位，`default` 或 `cheap`；不写就沿用该会话上一次执行的档位（从 `execution.started` 的 `model` 折叠，默认档位不写进记录），从没跑过的会话用 `default`。内联 `--settings` 由 `claude_settings(relay, profile)` 生成：Claude Code 把它合并在用户 settings 之上，键以内联为准，`env` 按名字逐个合并（2.1.318 实测：内联 `env` 只写基址和 haiku 槽位时，主模型和 `ANTHROPIC_AUTH_TOKEN` 仍来自 `~/.claude/settings.json`），所以网关 token 不经过 Spawn。配置了模型中继（`SPAWN_MODEL_RELAY`，跑 `spawn-model-relay` 的主机设为 `http://127.0.0.1:8791`；不设或 `off` 即不用中继，两档都不写 `env`，`cheap` 启动失败）时 `env` 写 `ANTHROPIC_BASE_URL` 指向中继、`ANTHROPIC_DEFAULT_HAIKU_MODEL` 为小模型（`SPAWN_SMALL_MODEL`，默认 `deepseek-flash`）；`cheap` 另把 `ANTHROPIC_MODEL` 与 opus、sonnet 槽位设为 `SPAWN_CHEAP_MODEL`（默认 `deepseek-v4-pro`）。每次启动前 Bridge 先连一下中继：连不上时 `default` 不写 `env`、照 Claude Code 自己的设置跑并在 tick 报告里警告，`cheap` 直接失败；Codex 不支持 `cheap`。

不注入 hook。新会话由 Bridge 用 `--session-id <uuid>` 命名并先记 `session.native_started`，恢复用 `--resume <uuid>`；Bridge 每个 tick 通过终端主机读取 `<Claude projects 目录>/<工作目录转义>/<uuid>.jsonl` 的新增整行（`SPAWN_CLAUDE_PROJECTS`），以引擎自带的 `uuid`、`tool_use_id` 作键去重。runner 把退出码写到 `SPAWN_EXIT_FILE`，Codex 输出另存到 `SPAWN_ENGINE_OUTPUT`；终端里没有数据库登录。

启动参数超过 7 个时按 `Run { engine, native, prompt }` 结构体传引用，保持签名可读。

checkpoint 规则：工作区有改动才 commit，成功后写 `artifact.committed`，`source_event_id` 使用 CommitId；`Unchanged` 不产生事件。

合入基线：`turn_stopped` 的 checkpoint 成功（含 `Unchanged`）且没有进行中的回合时，Bridge 紧接着调用 `merge_base`。回合是否进行由折叠决定：`message.received`、`tool.started` 打开，`turn.stopped`、`turn.failed` 关闭；Bridge 自己投递 `Input` 时也立即打开，本 tick 刚读到的会话记录行同样计入。此外，空闲的活执行（没有进行中的回合、本 tick 没读到要求 checkpoint 的事件、屏幕最后读到 Ready）按 Bridge 自己的时钟每 30 秒（`IDLE_MERGE_MS`）至多调用一次 `merge_base`；Bridge 投递提示（`Start` 的首个提示、`Input`）时立即打开回合。`Merged` 写 `artifact.committed`（reason `base_merged`，带合并改动的 `files`，折叠时不清掉仍待做的 checkpoint）；冲突按状态记一次 `merge.conflicted{files,marked}`，同一冲突或失败的警告只报一次，状态变了才再报。每个会话有一份由这两种事件折叠出的提示，每个文件记着最后提到它的合并事件的位置：Bridge 下一次为该会话键入 `Input` 或 `Resume` 提示时把它放在最前（「Spawn merged main into your workspace since your last turn: <files>. Re-read them before editing.」；冲突时说明这些文件带冲突标记、编辑标记区域、不要凭记忆重写），从不主动键入。键入成功后写 `notice.typed{through}`；只有这条提示到达引擎时才清空：会话记录里以通知固定开头的提示行产生 `notice.received`（Codex 的提示是启动参数，恢复执行的第一条提示即是它），只清掉 `through` 及之前的文件。别人键入的回合、引擎自己继续的回合、重启都不清空；已键入未到达的通知随下一条提示再键入。结束流程里最后一次 checkpoint 失败时不 terminate、不 unmount，执行保持活跃，下一个 tick 以同一原因重试（`Stop` 也是）；连续失败 20 次（`END_ATTEMPTS`）后结束进入停滞：杀掉终端、工作区保持挂载、写一次 `execution.end_stalled` 并大声告警一次，之后按 Bridge 的时钟每 30 秒（`END_RETRY_MS`）重试，重启后照旧，直到提交成功；Observer 把会话判为 BLOCKED，attention 原因 `end_stalled`。只有 `Discard` 会在 checkpoint 失败时卸载；`Conflicted` 与失败只进 tick 报告的警告（区分「目录里已有冲突标记」与「标记表达不了，未合并」），冲突由落地告诉 agent。

结束流程 `end`：最后一次 checkpoint → `terminate` → `unmount` → 写 Bridge 来源的 `execution.ended`，键 `<exec>:ended`。`terminate` 失败时不写事件，执行保持活跃，下一次 tick 重试。工作区只在最后一次 checkpoint 提交成功后卸载，或由 `Discard` 卸载。

结束的三个来源：

| 来源 | 触发 | `cause` |
| --- | --- | --- |
| `Stop`、`Discard` 命令 | 用户、supervisor | `Stopped`（已在结束中的执行保留原来的原因） |
| `spawn run` | 引擎退出，runner 把退出码写进 `SPAWN_EXIT_FILE`，Bridge 下一个 tick 读到后走结束流程 | `Exited`，带退出码 |
| tick 发现终端不在 | 进程崩溃或终端被外部杀掉 | `Lost` |

折叠时只有 Bridge 来源的 `execution.ended` 清除活跃执行；其他来源的同名事件只是退出报告（`agent_executions.exit_reported`）。

恢复：每次 `spawn tick` 是新进程，从位置 0 折叠全部事件得到活跃执行、原生会话 id 与待提交的 checkpoint。`Resume` 复用最新原生会话 id，新开 Execution 与 mount，优先 `--resume`；逻辑 Session 不变。

## 6. 屏尾判定

Claude 以 `--ax-screen-reader` 运行，屏幕是唯一可靠的完成信号来源。会话记录给结构化事件；屏尾只判两件事：能否输入、是否被阻塞。Bridge 每个 tick 读屏，两次读数一致才记录，忙碌状态每 30 秒重记一次。

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

`JudgeInput` 字段：`agent_id`、`session_id`、`engine`、`execution_active`、`stalled_end`、`last_end`、`open_tools`、`last_tool`、`permission_pending`、`screen`、`turn`、`last_error`、`evidence_seq`。`stalled_end` 来自当前执行的 `execution.end_stalled`（到 `execution.ended` 为止）：规则直接判 BLOCKED，attention 原因 `end_stalled`，不问判定器。

Observer 每次 tick：

1. 从游标读取增量，折叠成会话快照；控制流不进快照。
2. 规则（`rules`）先判：事实能定的状态直接成为结论，`judge = rules`。
3. 事实定不了的情形（`Ambiguity`）交给 `StateJudge`，附上歧义种类与规则的猜测；同一份证据（按指纹）不问第二次。
4. 规则的结论在状态或原因变化时才追加 `observation.recorded`；判定器的每次回答都追加，带证据与指纹。`based_on_seq` 记录依据。
5. `source = Observer` 的事件不进入快照；`observation.recorded` 只恢复上一次结论、`based_on_seq` 与指纹，新进程不重复写同一结论。

快照规则：引擎事件（工具、消息、回合）比屏尾读数新，到达时清空 `screen` 与 `permission_pending`。

判定实现：

| 实现 | 用途 |
| --- | --- |
| `HeuristicJudge` | 没有 Jev 地址（`SPAWN_JUDGE_URL`、`--judge-url`）时回答歧义情形的确定性判定器，离线可用，测试也用它；它不是规则层。`name()` 返回 `heuristic` |
| `JevJudge` | HTTP，见第 8 节。`name()` 返回 `jev` |

判定错误时写规则的猜测，原因 `JudgeUnavailable`、置信度 0、要求关注；不标记 Agent 死亡，按退避重试。

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

权限：视图只归 owner，public 只有 `agent_commands` 的 insert 与 `id` 列的 select；其他角色读 `security definer` 函数 `spawn_read_<视图>()`（state、status、tasks、freshness）与 `spawn_read_<视图>(session_id)`（timeline、judgements），按 `spawn_grants` 过滤 `session_user`，固定 `search_path`；`spawn_events`、`spawn_commands`、`spawn_grants`、`spawn_session_state` 撤销 public 全部权限；四个写函数撤销 public 执行权。`spawn_gateway_insert` 固定 `search_path = <schema>, pg_temp`，调用方无法用临时表替换 `spawn_grants`。

投影：`spawn_session_state` 每会话一行（最后序号、最后 id、事件数、最新 observation），由 `spawn_append` 在同一事务内更新，`session_seq` 守卫：行恰好落后一个事件时步进，否则从日志重折；迁移持 append 锁重折不一致的会话。会话归属取首个事件的 agent。

视图：`agent_state`（读投影，每会话最新 observation，带 `event_id`）、`agent_status`（判定加最近一次执行，`live` 标记未结束）、`agent_timeline`（不含 Observer 事件）、`agent_judgements`（只含 observation）、`agent_open_tools`、`agent_freshness`、`agent_lineage`（祖先链，深度上限 64）、`agent_task_events`（命令到事件）、`agent_tasks`（带 `prompt`）、`agent_executions`（只以 Bridge 的 `execution.ended` 为结束，`exit_reported` 标记 runner 已报告退出，带 `duration_ms`、`command_id`）、`agent_artifacts`、`agent_relations`、`agent_commands`。不用物化视图与 CDC：状态类视图读投影表，其余视图读时折叠。

`agent_commands` 上挂 `instead of insert` 触发器 `spawn_gateway_insert`，`security definer`，查 `spawn_grants`，无授权 raise `42501`。

## 10. tmux 与 git 契约

tmux：

| 调用 | 要点 |
| --- | --- |
| `new-session -d -s <name> -x 200 -y 50 -c <cwd> [-e K=V] -- prog …` | 每次执行一个会话，`-c` 设置起始目录 |
| `has-session`、`kill-session` | 会话 target：`=<name>` |
| `send-keys`、`capture-pane` | pane target：`=<name>:`，即当前窗口的活跃 pane；裸 `=<name>` 会被当作 pane 规格而报错，`:.0` 在 `pane-base-index 1` 下失效 |
| 重复会话 | stderr 为 `duplicate session: …`，适配层归一为 `already exists` |

git：一个裸仓库持有全部基线 commit，每个分支一份工作树。工作树由 `git archive` 导出，**不含 `.git`**，引擎读不到历史、其他分支和控制器的 ref。

`WorkspaceId` 是 `<branch>:<绝对目录>`；分支在冒号前，容器名由它派生为 `spawn-exec-<branch>`，任何进程只凭标识符就能重建映射。

| 方法 | 命令 |
| --- | --- |
| `ensure_branch` | 无 `refs/heads/main` 时用 `mktree` + `commit-tree` 建根提交；分支已存在返回 `Existing`；否则把 `from`（`refs/heads/<agent>` 或完整 commit id，`rev-parse --verify <x>^{commit}`）或 `main` 解析成 commit，`update-ref refs/heads/<b> <commit> ""`（只建不改） |
| `mount` | `git archive refs/heads/<b> \| tar -x -C <dir>`；配置了镜像时再 `docker run -d -v <dir>:<dir> -w <dir>` |
| `commit` | `GIT_INDEX_FILE=<root>/.index/<b> git add -A` → `write-tree`；树与父树相同则 `Unchanged`，否则 `commit-tree` + `update-ref` |
| `unmount` | `docker rm -f`，再删工作树目录 |
| `merge_base` | `main` 是分支头的祖先则 `Current`；否则 `git --attr-source=<main> merge-tree --write-tree --name-only -z <main> <head>`（合并规则取自 `main` 的 `.gitattributes`，只有 gate 能动它；裸仓库不加 `--attr-source` 不读树内属性）。退出码 1 为 `Conflicted`：消息种类（`-z` 输出里的稳定字符串）只有 `CONFLICT (contents)` 时，按下述同一安全流程把带冲突标记的合并树写进目录，分支不动，`main` 作为待定的第二父提交原子地写进 `<root>/.merge/<b>`（`main` 与冲突路径，NUL 分隔）；有别的冲突种类（modify/delete、改名、二进制）时什么都不改。待定合并存在时不合并更晚的 `main`：标记未解决返回 `Conflicted{marked: true}`，已解决返回 `Current`。`merge_base` 全程持有与 `commit` 相同的 `<index>.spawn-lock`；`mount`、`unmount` 都删掉待定文件（待定合并只属于写入它的那个目录）。`commit` 在待定合并列出的冲突文件里都没有完整的标记块（行首 `<<<<<<< `，其后一行恰为 `=======`，再其后行首 `>>>>>>> `；单独的 `=======` 不算）时以 `-p <head> -p <待定 main>` 提交（树未变也提交）并删掉待定文件，否则照常单父提交；干净时先动目录：临时索引 `read-tree <head 树>`、`update-index -q --refresh`、`read-tree -m -u <head 树> <合并树>`（目录里要改的文件与分支不符、或未跟踪文件挡路时拒绝，返回错误、什么都不改），再 `commit-tree -p <head> -p <main>`（保留 head 的 `Spawn-Large` trailer）和带旧值的 `update-ref`。顺序不能反：先动 ref 会让下一次 checkpoint 把旧文件提交回去，抹掉 `main` 的改动 |

积分器（integrator）在评判前拒绝带待定合并（`<root>/.merge/<agent>`）或改动文件里有完整标记块（`GitWorkspace::conflict_markers`）的分支：记 `conflict`，附「先解决冲突标记」的说明。

任务源（`spawn-cli` 的 `MissionWorkspaces`）在 git 存储外再包一层：每个任务源一个主机本地镜像 `<root>/.sources/<name>.git`。新分支前强制刷新镜像的 `main`，`merge_base` 前最多每 30 秒刷新一次。每次 checkpoint 产生新提交、合并或建分支后，分支被标记待推：后台线程把 `refs/heads/<agent>` 推到源的 `refs/heads/spawn/<agent>`，每个分支最多每 5 秒一次（推的是当时的分支头，中间丢的推送无所谓），从不强推（被拒说明有另一个写者：警告，不覆盖），只推给本机目录形式的源（中心存储）或 `SPAWN_PUSH_SOURCES` 逐字列出的远端（采用 `spawn/<agent>` 同此限制），失败按指数退避重试，存储被丢弃时（`spawn tick` 退出）同步推完。启动时镜像没有该分支则先从源取 `spawn/<agent>`，有就从它建（在另一台主机上续跑）；镜像里的分支落后于源上的则快进，分叉则警告并保留本地的。

容器把工作树挂到**同一绝对路径**，所以两侧路径一致，不需要任何翻译。

`docker_prefix` 为 `docker exec -i -t <容器> sh -c`，共 7 段；tmux 命令整体作为一段传入。

不用 FUSE，也不用 overlay：`git archive` 与 `tar` 在 macOS 和 Linux 上行为一致，无需 `SYS_ADMIN`。

## 11. 编译与磁盘策略

| 手段 | 配置 |
| --- | --- |
| 依赖最小化 | 无 async 运行时、无 chrono/uuid/rand、无 HTTP 客户端库 |
| 驱动隔离 | `postgres` 只在 `spawn-postgres`，HTTP 只在 `spawn-jev` |
| 工具链 | edition 2024，`rust-version` 1.95 |
| 调试信息 | dev `debug="line-tables-only"`、`split-debuginfo="packed"`；release `lto="thin"`、`codegen-units=1` |
| 编译缓存 | 开发时可用 kache；门禁沙箱清空环境，`deploy/deploy.sh` 置空 `RUSTC_WRAPPER`，二者都不用它 |
| 登录开销 | dev profile 对 `sha2`、`hmac`、`postgres-protocol` 开 `opt-level = 3`，SCRAM 登录从约 50 ms 降到约 6 ms |
| 目标目录 | 门禁的构建钩子把 `CARGO_TARGET_DIR` 固定在被测树内，两棵树不共用 |

## 12. 测试策略

库 crate 只测公开 API：断言位于各 crate 的 `tests/`（`api.rs` 等），库内不写 `#[cfg(test)]`。`spawn-cli` 是二进制，它的单元测试写在模块里。

| crate | 覆盖 |
| --- | --- |
| `spawn-core` | ID 校验、事件往返、去重、双序号、租约过期、屏尾六态、Bridge 事件序、每 tick 新进程折叠下的 Start→Stop→Start→Lost、Observer 跨进程不重复写 |
| `spawn-host` | git 分支创建、archive 导出、无变更判定、分支前移、容器前缀、tmux 精确名匹配与跨进程调用、真实 tmux 生命周期 |
| `spawn-jev` | 请求体结构与 criteria 顺序、真实响应往返、缺失字段回退、错误降级、chunked |
| `spawn-postgres` | schema 首次执行、幂等、稠密序号、单会话串行、租约重取、视图与读方法（列集快照、投影等于日志折叠、状态、时间线分页、判定、执行时长、未闭合工具、新鲜度、祖先链、命令事件、任务）、`spawn_finish` 四态、授权拒绝、非 owner 直查视图被拒、有授权写入与读函数过滤、共享一致性套件 |

共享一致性套件在 `spawn-core::contract`：`event_log`、`command_queue`、`workspace_store`、`terminal` 四个函数，任一实现都必须通过。

门禁：

| 步骤 | 命令 |
| --- | --- |
| 唯一门禁 | `scripts/rsi/gate.sh <repo> <base> <candidate> rsi/scenarios`：`rsi/scenarios/build` 构建，`check-clippy`、`check-tests`（需 `SPAWN_TEST_DATABASE_URL`）、`check-e2e`、`check-blackbox`、`check-web` 逐个必过 |
| 端到端 | `scripts/e2e.sh`：真 PG、真 git、真 tmux、脚本化引擎，覆盖整条决策链、任务、落地与目标任务 |
| 黑盒 | `scripts/blackbox/run-scenarios.sh`：CLI 契约、工作区导出（不含 `.git`）、提示落屏、视图与授权 |

缺少 `SPAWN_TEST_DATABASE_URL` 时 PG 测试显式 panic，不静默跳过。

## 13. 已知限制

| 项 | 限制 | 改法 |
| --- | --- | --- |
| 启动折叠 | `spawn tick` 每次、`spawn loop` 每次启动都从位置 0 折叠全部事件；耗时随事件数线性增长 | 按会话快照 |
| 租约续期 | Bridge 不续租；单条命令超过 `lease_ttl`（2 分钟）会被别的 worker 重取 | tick 内续租，或缩短单命令时长 |
| 重试上限 | `spawn_claim` 无最大 attempt，坏命令会被反复领取 | 加 `max_attempts`，超限直接 `finish(failed)` |
| schema 版本 | `migrate` 每次整体执行 `schema.sql`，无版本表 | 编号迁移文件与版本表 |
| 子进程超时 | `git`、`docker`、`tmux` 调用没有超时 | `wait_timeout` 或独立线程计时 |
| 屏尾报错 | `Error` 状态靠关键字匹配，误判可能 | 只用会话记录的 `turn.failed` |
| 视图读取 | 状态与新鲜度读投影表，其余视图读时折叠；迁移每次按会话聚合全量日志 | 日志很大时加迁移版本号，只跑一次，见 [view-architecture.md](./view-architecture.md) |
| 订阅 | 读取 `spawn_events` 增量；`LISTEN/NOTIFY` 未接入 | 接入 `LISTEN/NOTIFY` |
| 写入串行 | `spawn_append` 用一把全局 advisory lock | 按会话分锁 |
| 判定入参 | `JudgeInput` 的 serde 输出与真实服务入参是否语义等价未验证 | 用真实服务做契约测试 |
| 大文件 | 超过 8 MiB 的文件存在主机本地的块存储里，换一台主机就挂载失败 | 块存储放到 R2（见 [storage.md](./storage.md)） |
| tmux 输入 | `submit_text` 逐行发送，空串不发送任何内容 | `paste-buffer` |
| mount 泄漏 | mount 与 `execution.started` 之间崩溃会遗留一个 mount | 启动时清扫未记录的 mount |
