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

`spawn-core` 的 16 个模块：`bridge`、`command`、`contract`、`event`、`id`、`ingest`、`memory`、`mission`、`observation`、`observe`、`port`、`redact`、`screen`、`shell`、`time`、`transcripts`。

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

Session 与 Agent 一一对应：一个 Agent 只有一个 Session，一个 Session 只属于一个 Agent。`session_id` 只作为日志的列保留（日志不可改，各组件照旧传它），不是独立的实体。PostgreSQL 用 `spawn_agent_sessions(agent_id 主键, session_id 唯一)` 记下每个 Agent 首次写入的 Session，`spawn_append_trusted`（所有追加与提交都经过它）拒绝任何其他配对（`42501`）；靠的是两个唯一键，所以两个并发的首次写入不会都成功。迁移从已有日志回填，违反规则的配对以 warning 报出，保留各自的首个配对，日志不动。

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

`Source`：Claude、Bridge、Observer、Gateway、Control。读日志时，来源是本版本不认识的名字（已删除的写入方，例如 `layerfs`、`codex`）的记录被跳过，每个名字警告一次，读取不中断；`timeline` 照原样显示来源。

| 事件名 | 产生方 | 载荷 |
| --- | --- | --- |
| `session.native_started` | Bridge（新会话由它命名，引擎启动前记，不带版本）；会话记录（每个执行读到的第一行带 `version` 的会话记录行，键 `<exec>:engine_version`，恢复也记） | engine、native_session_id、transcript_path；可选 engine_version（引擎版本，取自 Claude Code 写在会话记录行上的 `version`；旧记录没有它照样能读，未设时不写） |
| `session.native_ended` | 无；保留它以读取日志里已有的行 | reason |
| `session.linked` | Bridge | parent_session_id |
| `message.received` | 会话记录（transcript） | 无 |
| `notice.typed` | 无；保留它以读取日志里已有的行（主线不再合入运行中的工作区） | through |
| `notice.received` | 无；保留它以读取日志里已有的行 | 无 |
| `notification.received` | 无；保留它以读取日志里已有的行 | notification_type |
| `tool.started` | 会话记录 | tool_name、tool_use_id |
| `tool.completed` | 会话记录 | tool_use_id、duration_ms |
| `tool.failed` | 会话记录 | tool_use_id、error |
| `turn.stopped` | 会话记录（`turn_duration`、中断） | input_tokens、output_tokens |
| `turn.failed` | 会话记录（API 错误行） | error |
| `job.started` | 会话记录：启动后台任务的工具结果（Bash 的 `toolUseResult.backgroundTaskId`，含超时或为送达消息被移到后台的命令；Monitor 的 `taskId`；后台 Agent 的 `agentId`，`isAsync` 且 `status` 为 `async_launched`，或派生到后台的 Skill，`status` 为 `forked`、`background`）；`SendMessage` 恢复已结束的 Agent（结果带 `resumedAgentId`）是同一 id 的又一次运行；键 `job:<id>:started:<tool_use_id>`，每次运行一条 | job_id、kind（`bash`、`monitor`、`agent`）；开始时间即记录时间 |
| `job.ended` | 会话记录：引擎的 `<task-notification>`（头部带 `<status>`，或 Monitor 的 `[Monitor expired …]` 事件），以引擎交给代理的那份为准：`queued_command` 附件或用户行（`queue-operation` 不读：它也排着子代理任务的通知，并可能早于启动该任务的工具结果写下）；Agent 的通知若说 `is waiting on its own background work`（暂停，自己的后台工作还在跑，会再通知）不算结束；成功的 `TaskStop` 结果；键 `job:<id>:ended:<通知摘要>`（两处副本字节相同，记一条；同一 Agent 下一次运行的通知不同，另记一条），`TaskStop` 为 `job:<id>:stopped:<tool_use_id>` | job_id、how（`completed`、`failed`、`killed`、`stopped`、`expired`，不认识的状态为 `ended`） |
| `execution.started` | Bridge | engine、workspace、command_id（终端总是 `spawn-<execution>`，由执行推出，不记录；旧记录里的 terminal 照常读入）；本次启动新建了分支时另有 fork（from：`main` 或启动的 `from`，跨主机删除之前的旧记录里还可能是 `spawn/<agent>`；commit：分支起点） |
| `execution.ended` | Bridge | cause、exit_code；Bridge 结束一个已无自己的 runner 在跑的执行时另带 evidence（主机看到了什么、哪条命令发现的） |
| `execution.unmounting` | Bridge（结束流程过了最后一次 checkpoint 与 terminate、卸载之前；键 `<exec>:unmounting`） | — |
| `command.step` | Bridge（命令处理的一步，键 `cmd_<id>:<step>`） | command_id、step（`mounted`、`mount_released`、`typing`、`typed`）、workspace（`mounted`/`mount_released` 带） |
| `execution.end_stalled` | Bridge（最后一次 checkpoint 连续失败 20 次，每个执行一次） | cause、exit_code、failures、error |
| `artifact.committed` | Bridge；`spawn-cli` 的 `main_merge.rs`（集成器的冲突裁决与投递前把 `main` 合入闲置工作区，source 记为 bridge） | reason（`tool_completed`、`turn_stopped`、`execution_ended`、`base_merged`）、artifact；`base_merged` 另带 files |
| `merge.conflicted` | Bridge（启动或恢复时合入 `main` 冲突，marked 为 false：分支不动）；`main_merge.rs`（把 `main` 合入闲置工作区冲突，内容冲突处写下冲突标记时 marked 为 true） | files、marked |
| `command.accepted` | Gateway 事务 | command_id |
| `command.finished` | Worker | command_id、succeeded、error |
| `screen.observed` | Bridge | state、evidence |
| `observation.recorded` | Observer | observation |
| `control.recorded` | 控制流（任务函数） | record（`{type, ...}`，见 fleet-architecture.md §10.1） |

`turn.stopped` 只表示模型本轮停止，不等于任务完成。任务完成只由 `SemanticState::Completed` 表达。

去重：`(session_id, source, source_event_id)` 唯一。Bridge 重读同一会话记录行返回既有记录，`inserted = false`。

脱敏：日志保存的自由文本（`tool.failed`、`turn.failed`、`execution.end_stalled`、`command.finished` 的 error，`screen.observed` 的 evidence，`observation.recorded` 里 observation 的 human_question、attention_reason 与 evidence 中的字符串）在追加前经 `spawn_core::redact::redact_event` 处理：`sk-…`、`ghp_…`（及 GitHub 其他 `gh?_` token）、`github_pat_…`、`AKIA…` 形状的密钥，`Bearer <token>` 的 token，`x-api-key` 头的值，URL userinfo 里的密码（`scheme://user:[redacted]@`），以及名字以 `PASSWORD`、`PASSWD`、`TOKEN`、`SECRET`、`KEY` 结尾（不分大小写）的赋值的值，都换成 `[redacted]`。两个存储（PostgreSQL 与内存实现）都在 `append` 与 `finish` 里做，任何写入方都绕不过去；标识符、路径和 `control.recorded`（任务源 URL、提示词会被折叠读回）不动。

## 4. 端口契约

`spawn-core::port` 定义六个 trait 与错误类型。

| trait | 方法 | 语义 |
| --- | --- | --- |
| `EventLog` | `append` | 追加一条事件，去重返回既有记录 |
| | `read_global(after, limit)` | `position > after` 的全部事件，位置可见性单调 |
| `CommandQueue` | `submit(principal, submission)` | 幂等提交，同时写 `command.accepted` |
| | `claim(worker, ttl)` | 领取最老且无活跃租约的会话首条命令 |
| | `finish(claimed, outcome)` | 记录结果并写 `command.finished` |
| `WorkspaceStore` | `ensure_branch(branch, from)`、`mount`、`commit`、`unmount` | 工作区的四个动作；`ensure_branch` 返回 `Existing` 或 `Created(Fork)`，新分支建在 `from`（另一个 agent 的分支头或完整 commit id，`ForkFrom::parse` 校验）或基线，已有分支忽略 `from`；`commit` 返回 `Created(CommitId)` 或 `Unchanged` |
| | `merge_base(workspace)` | 把基线（源的 `main`）合进已挂载、处于两轮之间空闲的工作区的分支（Bridge 不调用；集成器判 `conflict` 时、调度器给活着的 worker 键入任务前调用，见 spawn-cli 的 `main_merge`）：`Current`、`Merged{commit,files}` 或 `Conflicted{files,marked}`；内容冲突把带冲突标记的合并写进目录（`marked`），等标记解决后的 checkpoint 提交合并；目录与分支不符、或标记表达不了的冲突什么都不改。默认实现返回 `Current`（内存实现可配置结果） |
| `TerminalHost` | `start`、`submit_text`、`send_keys`、`is_alive`、`runner`、`capture_tail`、`read_from`、`write_new`、`terminate` | 终端生命周期、屏尾读取与读写引擎的文件（`write_new` 只建不改：路径上已有东西时返回 false，文件整体出现或不出现）；`start` 之后每个方法都再传一次 `WorkspaceId`，实现层不在进程间保存状态 |
| `TranscriptArchive` | `copies`、`read`、`append(copy, at, bytes)` | 原生会话记录的存档（见 [storage.md](./storage.md#the-transcript-archive)）：每个 agent 的每个原生会话有编号的副本，副本只增长，`at` 不等于副本当前长度的追加被拒，已存的字节从不改写；本地实现在 `<root>/.transcripts` |

错误类型：

| 类型 | 取值 |
| --- | --- |
| `StoreError` | `LeaseLost{command,attempt}`、`Rejected`、`Corrupt`、`Backend` |
| `WorkspaceError` | `Refused{code,message}`、`Unavailable` |
| `TerminalError` | 单字段消息 |
| `ArchiveError` | 单字段消息 |

并发规则：`append` 串行化；PG 实现用 `pg_advisory_xact_lock` 保证同库写入顺序，并用唯一索引兜底。

## 5. Agent Bridge

Bridge 做四件事：领取命令、启动与恢复执行、投递文本、提交工作区。

| 命令 | 行为 |
| --- | --- |
| `Start{engine,prompt,parent_session_id,from,model,charter}`（`charter` 是 supervisor 新原生会话所给章程的哈希，只供控制层记录，Bridge 不读） | 检查会话是否已有执行 → 开分支（`ensure_branch(branch, from)`；分支已存在时 `from` 被忽略并在 tick 报告里警告，`from` 解析不到 commit 时命令失败）→ 干净时把 `main` 合入分支（`merge_main_clean`，冲突时分支不动并记 `merge.conflicted`）→ mount，写 `command.step{mounted}` → 写 `execution.started`（新分支带 fork；这之前的追加失败时卸载并记 `mount_released`）→ 启动终端 → Claude 走屏尾等待投递。终端启动失败走结束流程，原因记为 `Lost` |
| `Input{text}` | Ready 才 `send-keys`；键入前写 `command.step{typing}`，成功后写 `{typed}`。屏尾非 Ready（或终端还活着、抓屏一直失败）时：人和 supervisor 的输入等轮次——写一次 `command.step{waiting}`（键 `<command>:waiting`），命令不结束，等下一次 Ready 读数再键入；循环自己的输入（principal 为 `spawn-dispatch`、`spawn-integrate`、`spawn-liveness`）照旧立即拒绝（`pane state <state> does not accept input`），排在等待中的输入之后的也立即拒绝。会话没有执行时立即失败 |
| `Stop` | 走结束流程，原因记为 `Stopped`；最后一次 checkpoint 失败时工作区不卸载 |
| `Discard` | 同 `Stop`，但最后一次 checkpoint 失败也卸载工作区，未提交的内容丢失；只由人或 supervisor 在 `execution.end_stalled` 之后发出 |
| `Resume{prompt,model}` | 复用最近的原生会话 id，新 Execution，逻辑 Session 不变 |

Claude Code 是唯一的引擎（`ENGINE` = `claude`）。`execution.started` 与 `session.native_started` 仍带 `engine` 字段，供 `agent_executions` 视图展示，代码不按它分支；记录里写着别的引擎名照样能读。`Start` 的 `engine` 可省略，写了就只能是 `claude`，否则命令解析失败。一次启动的参数由 `claude_arguments` 生成：

| 引擎 | 参数 |
| --- | --- |
| Claude | `--ax-screen-reader --allow-dangerously-skip-permissions --settings <内联 JSON> (--session-id <uuid> \| --resume <uuid>)` |

引擎模型（[fleet-architecture.md §3.3](fleet-architecture.md#33-engine-models)）：`Start`/`Resume` 的可选 `model` 是模型档位，`default` 或 `cheap`；不写就沿用该会话上一次执行的档位（从 `execution.started` 的 `model` 折叠，默认档位不写进记录），从没跑过的会话用 `default`。内联 `--settings` 由 `claude_settings(relay, profile)` 生成：Claude Code 把它合并在用户 settings 之上，键以内联为准，`env` 按名字逐个合并（2.1.318 实测：内联 `env` 只写基址和 haiku 槽位时，主模型和 `ANTHROPIC_AUTH_TOKEN` 仍来自 `~/.claude/settings.json`），所以网关 token 不经过 Spawn。配置了模型中继（`SPAWN_MODEL_RELAY`，跑 `spawn-model-relay` 的主机设为 `http://127.0.0.1:8791`；不设或 `off` 即不用中继，两档都不写 `env`，`cheap` 启动失败）时 `env` 写 `ANTHROPIC_BASE_URL` 指向中继、`ANTHROPIC_DEFAULT_HAIKU_MODEL` 为小模型（`SPAWN_SMALL_MODEL`，默认 `deepseek-flash`）；`cheap` 另把 `ANTHROPIC_MODEL` 与 opus、sonnet 槽位设为 `SPAWN_CHEAP_MODEL`（默认 `deepseek-flash`）。每次启动前 Bridge 先连一下中继：连不上时 `default` 不写 `env`、照 Claude Code 自己的设置跑并在 tick 报告里警告，`cheap` 直接失败。

不注入 hook。新会话由 Bridge 用 `--session-id <uuid>` 命名并先记 `session.native_started`，恢复用 `--resume <uuid>`；Bridge 监视 `<Claude projects 目录>/<工作目录转义>/<uuid>.jsonl`（`SPAWN_CLAUDE_PROJECTS`）和 runner 的退出文件，文件变了才读新增整行，以引擎自带的 `uuid`、`tool_use_id` 作键去重。runner 把退出码写到 `SPAWN_EXIT_FILE`；终端里没有数据库登录。

**Bridge 读什么，什么时候读。** 监视走 `FileWatch` 端口：Linux 上是 inotify（`spawn_host::InotifyWatch`），一个描述符、每个文件一条目录监视；路径还不存在时监视它上面最深的已存在目录，目录随引擎出现时监视跟着往下走（transcript 在 `<projects>/<工作目录转义>/`，退出文件在 `<工作区>/.spawn/`，两者都由引擎稍后创建）。设不了监视的执行（系统调用出错、文件系统不支持、`max_user_watches` 到顶）退回每个 pass 都读，并在循环输出里告警一次。读的偏移规则不变：只读整行，读到一半的行留到下一次变化或下一个 pass，绝不丢；一次 checkpoint 没提交的行同样留到下一个 pass。

一个 pass 对空闲的 agent 不花钱。没有命令在飞（Bridge 拿着在等，`held`）、transcript 没有变化、屏幕知道得够新的执行，这个 pass 不抓屏、不问 runner（一次 tmux `list-panes`）、不读文件。屏幕只在能改变决定时抓：

| 何时 | 出处 |
| --- | --- |
| 会话上有命令在飞（领取了没完成，或排队等它） | 投递等着它的读数 |
| transcript 自上次增长以来静止满 `SPAWN_SCREEN_QUIET_MS`（默认 500 ms）而屏幕此后还没读过，之后的 pass 接着读，直到连续两次读数一致 | transcript 说不了的事：提示符回来了 |
| 距上次抓屏满 `screen_refresh`（30 s）的兜底 | transcript 不动而屏幕变了：对话框、掉回 shell |

忙碌的屏幕仍每 30 秒重记一次。兜底抓屏一个 pass 最多 `BACKSTOPS_PER_PASS`（2）个，同时到期的屏幕分摊到后面几个 pass：一个 pass 抓完整个舰队会把下一条命令挡在后面。“引擎还在不在”不靠每个执行的 `runner`：有监视时，`TerminalHost::runners` 一个 pass 问一次终端服务器（tmux 一次 `list-panes -a`，整个舰队一次调用），终端不在或不再托管本主机的 runner 的执行，下一个 pass 就按 `lost` 结束，和以前一样快；没有监视时（`spawn tick`、测试）仍然逐个问。引擎自己退出照旧由退出文件的变化在半秒级发现。因果（§14）：屏幕状态只从抓屏来、一轮的结束只从 transcript 的行来；没抓屏、transcript 没变化都不算状态，transcript 静止只说明什么时候该再看屏幕。

启动参数超过 7 个时按 `Run { engine, native, prompt }` 结构体传引用，保持签名可读。

checkpoint 规则：工作区有改动才 commit，成功后写 `artifact.committed`，`source_event_id` 使用 CommitId；`Unchanged` 不产生事件。commit 失败（`Unavailable`、`Refused`）时告警，这次 checkpoint 仍然待做，下一个 tick 重试，直到提交成功（或得到 `Unchanged`）；闲着的 worker 不会再产生新的 checkpoint 事件，清掉它就等于丢掉这一轮的工作。

合入基线：只在执行启动或恢复时、挂载之前，Bridge 调用 `merge_main_clean`：`main` 能干净合入时分支按比较交换移到合并提交，写 `artifact.committed`（reason `base_merged`，带改动的 `files`）；有任何冲突时分支不动，执行照常在原分支上启动，写一次 `merge.conflicted{files, marked: false}`（键由分支头与文件决定，重试的启动只记一次），并在 tick 报告里告警。Bridge 从不合入运行中的执行；闲置在提示符的工作区由 `main_merge.rs` 在集成器的冲突裁决和投递任务之前合入，投递的任务文本说明合并做了什么（storage.md，“Main into a workspace”）。结束流程里最后一次 checkpoint 失败时不 terminate、不 unmount，执行保持活跃，下一个 tick 以同一原因重试（`Stop` 也是）；连续失败 20 次（`END_ATTEMPTS`）后结束进入停滞：杀掉终端、工作区保持挂载、写一次 `execution.end_stalled` 并大声告警一次，之后按 Bridge 的时钟每 30 秒（`END_RETRY_MS`）重试，重启后照旧，直到提交成功；Observer 把会话判为 BLOCKED，attention 原因 `end_stalled`。只有 `Discard` 会在 checkpoint 失败时卸载。

结束流程 `end`：最后一次 checkpoint → `terminate` → `unmount` → 写 Bridge 来源的 `execution.ended`，键 `<exec>:ended`。`terminate` 之后、`unmount` 之前写 `execution.unmounting`；恢复出的结束若已有这条记录，就跳过 checkpoint，只再 terminate、unmount（工作区不在不算失败）并写 `execution.ended`。

命令是日志里记账的幂等步骤：重试的命令（租约过期或 `finish` 失败）与重启后的 Bridge 都按日志续做，不重复副作用。`Start`/`Resume` 重试时若 `execution.started` 已是本命令的执行，就续做：终端不在、或不托管本主机启动的 runner 则失败（下个 tick 按 `Lost` 结束），绝不往这样的终端里键入，提示词已有 `typed`、或 `typing` 之后转录里已有这次执行的提示，则成功；只有 `typing` 而无从判断时不再键入，结束这次执行并失败（提示词可能已到引擎，所以不按 discard 结束：最后一次 checkpoint 失败时工作区不卸载，结束像 `Stop` 一样留待下个 tick 重试；键入失败时同理，只有日志表明从未开始键入的启动才按 discard 卸载）；没有 `typing` 才键入。本命令的执行已结束则失败，不再启动。`Input` 同理：有 `typed` 即成功，只有 `typing` 时失败且不再键入（至多一次，错误含 `MAY_HAVE_TYPED`）。等待中的输入也是这样：等待记在日志里（`waiting`），重启后的 Bridge 接着等；一次 Ready 读数只给一次键入用，下一条输入要等上一条 `typed` 之后的新 Ready 读数（或那次键入开始的轮次已结束）；同一会话的等待输入按 id 从旧到新键入，前面还有在等的就不键入后面的。等待的输入不挡 `Stop`、`Discard`、`Resume`：`spawn_claim` 跳过有 Bridge `waiting` 记录的未完成命令，把它后面的命令发出来（后面的输入也会发出来，以便记下自己的 `waiting`）；没有 `waiting` 记录的未完成命令仍挡住后面的一切。等待本身没有超时：Bridge 在本 tick 的领取循环结束前一直拿着它的租约，然后用 `spawn_release` 放开，`input_wait_retry`（1 秒）后可再领。执行结束时（同一 tick 或下一 tick），它的等待输入失败：`Stop`/`Discard` 结束的报 “the session was stopped before the input was typed”，其他结束报 “the session's execution ended (<cause>) before the input was typed”；`Resume` 不会让它们复活。`waiting` 不算活动：停滞层（`stall_facts` 的会话最新事实）和调度器的判定新鲜度（`dispatch_facts` 的 `evidence_seq`）都不计它，Observer 把它当普通命令步骤，之后的判定覆盖它。启动时 Bridge 重放日志后，对记了 `mounted`、没有对应 `execution.started` 也没有 `mount_released`、且没有活跃执行在用的工作区，卸载并补记 `mount_released`；日志仍认为这条命令的执行（或用同一工作区的执行）未结束时不算孤儿，保留挂载并重新跟踪该执行。`terminate` 失败时不写事件，执行保持活跃，下一次 tick 重试。工作区只在最后一次 checkpoint 提交成功后卸载，或由 `Discard` 卸载。

结束的三个来源：

| 来源 | 触发 | `cause` |
| --- | --- | --- |
| `Stop`、`Discard` 命令 | 用户、supervisor | `Stopped`（已在结束中的执行保留原来的原因） |
| `spawn run` | 引擎退出，runner 把退出码写进 `SPAWN_EXIT_FILE`，Bridge 下一个 tick 读到后走结束流程 | `Exited`，带退出码 |
| tick 发现终端不在 | 进程崩溃或终端被外部杀掉 | `Lost` |

折叠时只有 Bridge 来源的 `execution.ended` 清除活跃执行。其他来源的同名事件只是退出报告（`agent_executions.exit_reported`），现在已无人写：runner 把退出码写进 `SPAWN_EXIT_FILE`，所以只有那之前的旧执行可能为真。

每会话一个活跃执行由日志保证，不只靠 Bridge 的内存：`Start`/`Resume` 先追上日志再查内存，日志的追加再查一次（`spawn_append_trusted`，内存实现同样）：会话最近一次执行还没有 Bridge 写的结束时拒绝 `execution.started`。被拒的启动以日志给的原因失败，卸载自己刚挂的工作区（那正是另一个执行的工作区时不卸载），不启动终端。runner 的退出报告不算结束；丢失的执行由下一个 tick 写 `execution.ended`（cause `lost`）后，会话即可再启动或恢复。

“丢失”指没有 Bridge 自己启动的 runner 在跑它（issue #15）：`TerminalHost::runner` 回答 `Running`（窗格活着，且由本主机的启动拉起：tmux 的 `pane_start_command` 带有本次启动的环境文件名 `spawn-launch-<终端>-`）、`Gone`（终端不在）或 `Untracked`（终端在，但不托管那个 runner：窗格已死，或重启后终端管理器恢复出、停在 shell 提示符的同名会话）。只有 `Running` 算在运行；其余先再读一次退出文件，没有就按 `lost` 结束，`evidence` 记下主机所见。`Start`、`Resume`、`Input` 动手前先处理会话里这样的执行（`Bridge::settle`，走正常的结束流程），而不是因为它失败、等以后的 tick：start/resume 接着做，input 失败一次（需要运行中的引擎），之后照常 resume；`Stop` 写下日志在等的结束（`stopped`，同样带 evidence）。Bridge 的视图只随折叠日志改变（它写的结束被读回后，执行才离开视图）；视图里会话没有执行时，命令向日志查询（`EventLog::open_execution`，与守卫同一规则：最近一次 `execution.started` 之后没有 Bridge 写的结束），查到就重新跟踪并警告。重启时，视图没读到其启动、但日志仍认为未结束的执行所用的挂载不算孤儿，不卸载。不受跟踪的引擎从不被静默收编：它被结束，终端被杀。

恢复：每次 `spawn tick` 是新进程，从位置 0 折叠全部事件得到活跃执行、原生会话 id 与待提交的 checkpoint。`Resume` 复用最新原生会话 id，新开 Execution 与 mount，优先 `--resume`；逻辑 Session 不变。

会话记录存档（`spawn_core::transcripts`）：每次 `turn_stopped` 的 checkpoint 之后、结束流程里 `terminate` 之后 `unmount` 之前，Bridge 把会话记录自上次复制以来新增的字节追加进 `TranscriptArchive`；最后复制的 4 KiB 与文件同一位置不一致（被截短、改写或替换）时整份复制成下一个副本，旧副本不动。`Resume` 在 mount 之后、启动终端之前检查会话记录：缺失时从最新副本恢复（tick 报告里警告）；缺失且没有副本、而日志里有从它读出的事件时，恢复失败并卸载，不在旧 id 下空白启动。日志不存会话记录内容，也不记复制。

## 6. 屏尾判定

Claude 以 `--ax-screen-reader` 运行，屏幕是唯一可靠的完成信号来源。会话记录给结构化事件；屏尾只判两件事：能否输入、是否被阻塞。Bridge 不是每个 tick 都读屏（§5）：会话上有命令在飞、transcript 静止满 `SPAWN_SCREEN_QUIET_MS`、或距上次抓屏满 30 秒的兜底时才读，两次读数一致才记录；兜底读到的与已记录的一样时不重复记录，忙碌状态每 30 秒重记一次。

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

`JudgeInput` 字段：`agent_id`、`session_id`、`engine`、`execution_active`、`stalled_end`、`last_end`、`open_tools`、`open_jobs`、`oldest_job_ms`、`last_tool`、`permission_pending`、`screen`、`turn`、`last_error`、`evidence_seq`。`stalled_end` 来自当前执行的 `execution.end_stalled`（到 `execution.ended` 为止）：规则直接判 BLOCKED，attention 原因 `end_stalled`，不问判定器。`open_jobs` 是当前执行里最近一次 `job.started` 之后没有 `job.ended` 的后台任务数（执行结束或新执行开始时清零；没有时不写进证据）：回合停在提示符（`turn.stopped` 或 Ready 屏）而 `open_jobs > 0` 时规则直接判 WORKING，原因 `WAITING_FOR_BACKGROUND_JOB`，不问判定器（[fleet-architecture.md](fleet-architecture.md) §2.2）；任务开了多久不改变这个结论，上限由调度器管（`SPAWN_STALL_JOB_SECS`）。

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
| `JevJudge` | HTTP，见第 8 节。`name()` 返回 `jev` |

判定错误时写规则的猜测，原因 `JudgeUnavailable`、置信度 0、要求关注；不标记 Agent 死亡，按退避重试。没有 Jev 地址（`SPAWN_JUDGE_URL`、`--judge-url`）时 Observer 只用规则（`Observer::new` 不带 `with_judge`）：规则定不了的情形照判定器不可用处理，记下歧义与规则的猜测，`judge` 为 `none`（`NO_JUDGE`），要求关注，由 dispatcher 上报而不是催促。日志里 `judge` 为 `heuristic`（已删除的判定器）的判定照常读取和显示。

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
| `spawn_agent_sessions` | `agent_id` 主键、`session_id` 唯一：Agent 与 Session 一一对应（§2） |

函数：

| 函数 | 返回 | 说明 |
| --- | --- | --- |
| `spawn_now_ms()` | `bigint` | 单一时间源 |
| `spawn_append(...)` | `out_id`、`out_seq`、`out_at`、`out_inserted` | 事务内取序号并插入，`pg_advisory_xact_lock(1399957335)` 串行化；会话最近一次 `execution.started` 之后还没有 Bridge 写的同一执行的 `execution.ended` 时，再一条 `execution.started` 被拒（`23P01`，`StoreError::Rejected`），同键重放照旧返回既有记录 |
| `spawn_submit(...)` | `out_id` | 幂等插入并写 `command.accepted` |
| `spawn_claim(worker, ttl_ms)` | `out_id`、`out_attempt`、`out_agent`、`out_session`、`out_principal`、`out_action` | `for update skip locked`，单会话串行（有 Bridge `waiting` 记录的未完成输入不挡后面的命令），过期租约 attempt+1 |
| `spawn_release(command, attempt, ms)` | `boolean` | 不结束命令，把租约改为 `ms` 毫秒后到期；旧 attempt 或已结束的命令不动 |
| `spawn_finish(command, attempt, succeeded, error)` | `done`、`already`、`stale`、`missing` | 旧 attempt 结束被拒 |

权限：视图只归 owner，public 只有 `agent_commands` 的 insert（不能 select，连 `id` 列也不能，否则能列出所有命令 id；提交方从 `spawn_gateway_submit(agent, session, key, action)` 拿回自己那条命令的 id）；其他角色读 `security definer` 函数 `spawn_read_<视图>()`（state、status、tasks、freshness）与 `spawn_read_<视图>(session_id)`（timeline、judgements），按 `spawn_grants` 过滤 `session_user`，固定 `search_path`；`spawn_events`、`spawn_commands`、`spawn_grants`、`spawn_session_state` 撤销 public 全部权限；写函数（含 `spawn_release`）撤销 public 执行权。`spawn_gateway_insert` 固定 `search_path = <schema>, pg_temp`，调用方无法用临时表替换 `spawn_grants`。

投影：`spawn_session_state` 每会话一行（最后序号、最后 id、事件数、最新 observation），由 `spawn_append` 在同一事务内更新，`session_seq` 守卫：行恰好落后一个事件时步进，否则从日志重折；迁移持 append 锁重折不一致的会话。会话归属取首个事件的 agent。

视图：`agent_state`（读投影，每会话最新 observation，带 `event_id`）、`agent_status`（判定加最近一次执行，`live` 标记未结束）、`agent_timeline`（不含 Observer 事件）、`agent_judgements`（只含 observation）、`agent_open_tools`、`agent_freshness`、`agent_lineage`（祖先链，深度上限 64）、`agent_task_events`（命令到事件）、`agent_tasks`（带 `prompt`）、`agent_executions`（只以 Bridge 的 `execution.ended` 为结束，`exit_reported` 标记旧 runner 以事件报告过退出（现在不再写，新执行总为假），带 `duration_ms`、`command_id`）、`agent_artifacts`、`agent_relations`、`agent_commands`。不用物化视图与 CDC：状态类视图读投影表，其余视图读时折叠。

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

`WorkspaceId` 是 `<branch>:<绝对目录>`；分支在冒号前，任何进程只凭标识符就能重建映射。

| 方法 | 命令 |
| --- | --- |
| `ensure_branch` | 无 `refs/heads/main` 时用 `mktree` + `commit-tree` 建根提交；分支已存在返回 `Existing`；否则把 `from`（`refs/heads/<agent>` 或完整 commit id，`rev-parse --verify <x>^{commit}`）或 `main` 解析成 commit，`update-ref refs/heads/<b> <commit> ""`（只建不改） |
| `mount` | 分支头的提交说明带 `Spawn-Large` trailer（旧版本把大文件放在块存储里，本版本没有块存储）时拒绝（`FilesOutsideGit`）；否则 `git archive refs/heads/<b> \| tar -x -C <dir>`，先解到 `<root>/.mount/` 再改名换上 |
| `commit` | `GIT_INDEX_FILE=<root>/.index/<b> git add -A`（超过 `SPAWN_MAX_FILE_MB` 的文件排除在外、不哈希，`main` 里已有的 blob 除外）→ `write-tree`；树与父树相同则 `Unchanged`，否则 `commit-tree` + `update-ref`。被排除的文件记在 `<root>/.oversized/<b>`，`notices` 只读出尚未告知的（不改记录），Bridge 记进日志并放在下一个 prompt 前，这次键入记下 `typed` 之后才用 `notices_told` 标为已告知；记录在磁盘上，Bridge 重启不丢（至多告知两次） |
| `unmount` | 删工作树目录 |
| `merge_base` | `main` 是分支头的祖先则 `Current`；否则 `git --attr-source=<main> merge-tree --write-tree --name-only -z <main> <head>`（合并规则取自 `main` 的 `.gitattributes`，只有 gate 能动它；裸仓库不加 `--attr-source` 不读树内属性）。退出码 1 为 `Conflicted`：消息种类（`-z` 输出里的稳定字符串）只有 `CONFLICT (contents)` 时，按下述同一安全流程把带冲突标记的合并树写进目录，分支不动，`main` 作为待定的第二父提交原子地写进 `<root>/.merge/<b>`（`main` 与冲突路径，NUL 分隔）；有别的冲突种类（modify/delete、改名、二进制）时什么都不改。待定合并存在时不合并更晚的 `main`：标记未解决返回 `Conflicted{marked: true}`，已解决返回 `Current`。`merge_base` 全程持有与 `commit` 相同的 `<index>.spawn-lock`；`mount`、`unmount` 只在分支已带着标记（合并写入后又有 checkpoint）时保留待定文件，否则删掉。`commit` 在待定合并列出的冲突文件里都没有完整的标记块（行首 `<<<<<<< `，其后一行恰为 `=======`，再其后行首 `>>>>>>> `；单独的 `=======` 不算）时以 `-p <head> -p <待定 main>` 提交（树未变也提交）并删掉待定文件，否则照常单父提交；干净时先动目录：临时索引 `read-tree <head 树>`、`update-index -q --refresh`、`read-tree -m -u <head 树> <合并树>`（目录里要改的文件与分支不符、或未跟踪文件挡路时拒绝，返回错误、什么都不改），再 `commit-tree -p <head> -p <main>` 和带旧值的 `update-ref`。顺序不能反：先动 ref 会让下一次 checkpoint 把旧文件提交回去，抹掉 `main` 的改动 |

积分器（integrator）在评判前拒绝带待定合并（`<root>/.merge/<agent>`）或改动文件里有完整标记块（`GitWorkspace::conflict_markers`）的分支：记 `conflict`，附「先解决冲突标记」的说明。推上来的分支与 `main` 冲突时，积分器先经 `MissionWorkspaces::merge_base` 把 `main` 合进该 agent 空闲的工作区（内容冲突写标记、`main` 待定），再记 `conflict`，说明里点名带标记的文件；标记表达不了的冲突把 `main` 的版本复制到工作区的 `.spawn/main/`，说明逐个文件该怎么做。调度器给活着的 worker 键入任务前也这样合并一次，任务文本写明合并结果。

语义冲突：gate 拒绝了 C，而 C 是真正的合并（推上来的分支头不含 `main`）时，积分器在记任何裁决之前，用同一个 gate、同样的场景单独评判分支头：base 为 `git merge-base <head> <main>`（`head_base`），candidate 为分支头本身。分支头通过：记 `incompatible`，像 `conflict` 一样把任务送回（`revise`，修订数加一），不计入三次拒绝的停机；记录写明 `candidate`、`head`、`head_base`、`head_verdict`、`main_commits`（`head_base..base` 的 `main` 首父提交）和引用 gate 输出、列出这些提交的说明。分支头也不通过：`rejected`，照旧计数。分支头的评判没有裁决：`aborted`。判定只凭两次记录在案、针对具名提交的 gate 裁决（§14），从不读理由的文字。

落地提交：每次落地都让 `main` 恰好多一个新提交 C，`git commit-tree <树> -p <base> -p <推上来的分支头> -m <说明>`，作者与提交者都是 `spawn-integrate`，不签名；`main` 是分支头的祖先时也一样，从不快进到 worker 的提交。树：`main` 是分支头的祖先时取分支头的树，否则取 `--attr-source=<main> merge-tree --write-tree <main> <head>` 的合并树（冲突仍记 `conflict`）。C 在 gate 之前造好：gate 评判的就是它，`task.gated` 的 `candidate`、推送目标和 `task.landed` 的 `to` 都是它；`--force-with-lease=<main>:<base>` 不变，`main` 动过仍记 `stale`。说明只取自记录（任务的 brief 与 report），从不读工作区：首行为目标标题、目标号与任务号（`<标题> (objective <id>, task <id>)`；无目标的任务为 brief 第一行非空文本加 `(task <id>)`），空一行后是 report 的 `## What changed` 一节（没有这一节则取整份 report，至多 3000 字符；其中以 `Spawn-` 开头的行缩进，不会变成 trailer，`Spawn-Large` 也就不会让 `main` 无法挂载），最后是 `Spawn-Task`、`Spawn-Objective`、`Spawn-Head` trailer。于是 `git log --first-parent main` 每行一个落地的任务，`git bisect start --first-parent`（git 2.29+）只停在经过 gate 的落地上（普通 bisect 可能停在未经 gate 的 checkpoint 上，这是保留历史的代价），`git revert -m 1 <C>` 恰好撤销一个任务；worker 的 checkpoint 经第二父提交仍可从 `main` 到达，所以被撤销的目标以新任务从当前 `main` 重来（或撤销那次撤销），而不是再合并同一分支。用合并提交而不用 squash：历史不需要另一个 ref 来保留，分支头在 `main` 里，下一次把 `main` 合进该分支时干净。

任务源（`spawn-cli` 的 `MissionWorkspaces`）在 git 存储外再包一层：每个任务源一个主机本地镜像 `<root>/.sources/<name>.git`。新分支前强制刷新镜像的 `main`，`merge_base` 前最多每 30 秒刷新一次。每次 checkpoint 产生新提交、合并或建分支后，分支被标记待推：后台线程把 `refs/heads/<agent>` 推到源的 `refs/heads/spawn/<agent>`，每个分支最多每 5 秒一次（推的是当时的分支头，中间丢的推送无所谓），从不强推（被拒说明分支被改写：警告，不覆盖），只推给本机目录形式的源（远端可能公开，推了每个提交都会离开本机），失败按指数退避重试，存储被丢弃时（`spawn tick` 退出）同步推完。推上去的 `spawn/<agent>` 供集成器落地。分支只在本机：启动时除 `main` 外不从源取任何东西，不从别的主机接手分支（所有者的决定：Spawn 只在一台主机上跑）。

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
| `spawn-host` | git 分支创建、archive 导出、无变更判定、分支前移、tmux 精确名匹配与跨进程调用、真实 tmux 生命周期 |
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

门禁在 base 上的 objective 结果（solved，或两次一致的 unsolved）缓存在沙箱看不到的 `~/.cache/spawn/gate-base`，键恰好是这些：base commit；场景文件的 blob；`gate.sh` 与它引入的 `pg-url.sh` 的 blob；场景目录里除场景（`*.sh`）、`protected`、`retired-tests` 以外的文件（构建钩子、`bind` 列表的文本、场景旁的帮助脚本）；`bind` 列表里每个目录的指纹（有 `.modules.yaml` 或 `.pnpm/lock.yaml` 时取它们的 blob，否则取目录下全部条目的路径、类型、大小、修改时间与链接目标，不跟随符号链接）；场景代码行（不含注释）点名的文件及它们再点名的文件（控制器的 `scripts/...`、同目录的 `<name>.sh`；经变量或 `cd` 间接进入 `scripts/` 时取整个 `scripts/`）；沙箱里在 base 树内问到的 `rustc -vV`、`cargo -V`、`node --version`、`psql -V`、`git --version`、`tmux -V`、`python3 -V`、`jq --version` 与测试服务器的 `server_version_num`。不在键里的：有安装记录的目录里未经安装的单个文件改动；候选代码留在共享测试服务器上的状态（角色、库、设置）——每个门禁一个一次性测试服务器是 GitHub issue #6，在那之前集成器一次只跑一个门禁。只有门禁在一次完整的 base 运行后写条目；读不出的条目算未命中；候选的运行从不缓存；命中会 touch 条目，`spawn-maintenance` 删除 14 天没用过的条目。

## 13. 已知限制

| 项 | 限制 | 改法 |
| --- | --- | --- |
| 启动折叠 | `spawn tick` 每次、`spawn loop` 每次启动都从位置 0 折叠全部事件；耗时随事件数线性增长 | 按会话快照 |
| 租约续期 | Bridge 不续租；单条命令超过 `lease_ttl`（2 分钟）会被别的 worker 重取 | tick 内续租，或缩短单命令时长 |
| 重试上限 | `spawn_claim` 无最大 attempt，坏命令会被反复领取 | 加 `max_attempts`，超限直接 `finish(failed)` |
| schema 版本 | `migrate` 每次整体执行 `schema.sql`，无版本表 | 编号迁移文件与版本表 |
| 子进程超时 | `git`、`tmux` 调用没有超时 | `wait_timeout` 或独立线程计时 |
| 屏尾报错 | `Error` 状态靠关键字匹配，误判可能 | 只用会话记录的 `turn.failed` |
| 视图读取 | 状态与新鲜度读投影表，其余视图读时折叠；迁移每次按会话聚合全量日志 | 日志很大时加迁移版本号，只跑一次，见 [view-architecture.md](./view-architecture.md) |
| 订阅 | 读取 `spawn_events` 增量；`LISTEN/NOTIFY` 未接入 | 接入 `LISTEN/NOTIFY` |
| 写入串行 | `spawn_append` 用一把全局 advisory lock | 按会话分锁 |
| 判定入参 | `JudgeInput` 的 serde 输出与真实服务入参是否语义等价未验证 | 用真实服务做契约测试 |
| 大文件 | 超过 `SPAWN_MAX_FILE_MB`（默认 100）的文件不进 checkpoint，卸载即丢 | 需要时再加外部存储（见 [storage.md](./storage.md)） |
| tmux 输入 | `submit_text` 逐行发送，空串不发送任何内容 | `paste-buffer` |
| mount 泄漏 | mount 与 `execution.started` 之间崩溃会遗留一个 mount | 启动时清扫未记录的 mount |

## 14. 跨循环的因果不变式

Bridge、Observer、调度器（`deliver.rs`、`dispatch.rs`）、集成器（`integrate.rs`）与 supervisor 各自按节拍运行，只经同一个 PostgreSQL 日志协作。它们之间只有一条规则：

**一个循环不凭时间、缺席或“最新状态”推断另一个循环的进度；它只依据一条写明自己覆盖了什么的记录行动（一个 commit、一个会话位置、一个命令 id）。每个副作用（命令、合并、checkpoint、投递、停止、推送）都有自己记下的结果，消费方检查结果：打算做不等于做了。**

违反它的五种形状，以及审计（2026-11-05，`REPORT.md` 在分支 `causality-audit`）找到的例子：

| 形状 | 例子 |
| --- | --- |
| 打算当作做了 | `last_delivery` 把失败的投递算作工人收到的最后一个任务，挡住它自己的重试；失败的 `continue` 被当成“催了也没用”；取消的任务，工人从未被告知；集成器的预算中止只提交 Stop 不看结果；推送意图只在进程内存里，进程重启即丢，集成器之后一直判 `stale` |
| 最新状态当作另一件事的证据 | 报告在它那一轮的 checkpoint 之前被收取，于是对着更早的 commit 判成“没有改动”；门禁结论落到任务当前的报告上，而不是它评判的那一版；`task.landed` 不看 `from` 就移动其他任务的基线；镜像拉取失败时把旧的 `main` 当成源的 `main` |
| 缺席当作状态 | Observer 只在结论变化时写，后来的事实让调度器没有当前判定；超限文件提示在打进任何提示词之前就记成“已告知” |
| 循环之间假设先后 | 所有者推送与它的 `mission moved` 之间跑了一趟集成器，误判停摆（已修复：所有者用 `spawn mission push` 落地，见下文）；落地推送成功而 `task.landed` 未记时，同源的其他任务据 `main` 行动 |
| 生产者已删，消费者还在等 | 注释承诺“轮次结束时合入 main”，而闲置自动合入早已删除（冲突死锁）；跨主机推送删掉后，远程源上带门禁的任务永远 `stale` |

审计第 1、2、3、9、16、23 条已修复（分支 `audit-stops`）：失败的 Stop，不论谁发的，调度器都从命令队列读出当前执行的 Stop 及其结果，换新键重发、等待加倍，三次后升级给 supervisor（`dispatch/stops.rs`）；集成器先提交中止的 Stop 再记 `task.aborted`；没打进去的 nudge 不算催过；人发的命令完成后才算消息，失败的照实说；`done-<episode>` 看记下的 `end_cause`；`dispatched` 保留当前回合的 nudge 与当前执行的 resume，关闭的临时任务在工人仍在跑时留在投递行里，批准与人的消息直到告知为止都算消息：不再凭 24 小时窗口判断“没发过”或“不是消息”。

所有者的改动同样受它约束：线上的门禁脚本、场景、objective backlog 与维护脚本都来自控制器 `~/.local/share/spawn-rsi/controllers/<name>`（部署的那个提交的树，部署时原子切换，`deploy/controller.sh`），从不来自所有者的工作检出。否则检出里每次保存都立即生效，门禁还没评判过（曾经：改 gate.sh 改变了一个正在运行的门禁；提交到 `rsi/backlog` 的 objective 在部署前几秒内就被分派）。运行的东西必须是被评判过的东西的记录：**所有者的改动部署后才生效**。

后台任务同样守这条规则（[fleet-architecture.md](fleet-architecture.md) §2.2）：工人是否在等自己的后台任务，只依据 Bridge 从会话记录读出的 `job.started` 与 `job.ended`（以及结束它所在执行的 `execution.ended`），从不依据屏幕上的 “N shells still running” 或 “N monitors still running”：屏尾只作为 Jev 看到的证据。停止任务要看 `TaskStop` 的结果说它停了，不看发出的调用（打算不等于做了）；任务开了多久用数据库的时钟（`spawn_now_ms()` 对 `job.started` 的 `recorded_at_ms`）。

所有者部署时暂停落地，同样是记录，它的结束也是（objective 34，[fleet-architecture.md](fleet-architecture.md) §10.3 “Landing: the owner's hold”）。`spawn mission hold --source <源>` 在集成器的落地锁下读源的 `main`（已开始推送的落地先记下结果），记 `source.held`（`main`、`by`、`bound_s`、`reason`，存储写入 `expires_ms` = 记录自己的时间 + `bound_s` × 1000）；结束它的只有一条记录：所有者的 `source.released`，或集成器在数据库时钟（`spawn_now_ms()`）到达 `expires_ms` 之后、每一轮开始时记下的 `source.hold_expired`（存储在此之前、或对已不是当前的暂停一律拒绝，先记下的释放或续期胜出）。集成器的 `land()` 在落地锁下、推送之前从存储读暂停，从不凭内存或时间：被暂停的源上不推送，每个任务每个暂停记一次 `task.held`，任务仍是 `landing`；它的门禁照跑并记下结论，它所在任务组的这一步照常做完基线检查与目标分派，这个源不算“动过”；别的源、停机、预算与 `mission moved` 照常。过了 `expires_ms` 而还没有过期记录时什么也不落地：结束暂停的是记录，不是时钟。没有哪一步依赖哪个循环先跑。所有者的门禁前先暂停 Spawn 源（`deploy/hold.sh`），`deploy/deploy.sh` 在每条退出路径上释放（没有暂停时释放什么也不记），不再为整个门禁停掉 `spawn-integrate`。

所有者推到任务组来源上的改动也是一条记录（objective 21，[fleet-architecture.md](fleet-architecture.md) §10.3 “The owner's landings”）。从前的做法是停掉 spawn-integrate、推送、`spawn mission moved`、再启动：停集成器会杀掉所有任务组、所有来源上正在跑的门禁（2026-11-06 任务 48 因此丢了一个 40 分钟的门禁）；不停就推，集成器的一轮可能在 `mission moved` 记下之前读到新的 `main`，把任务组判成停摆（上表“循环之间假设先后”的例子）。所有者的落地对集成器来说只是一种缺席，不是它能据以行动的记录。现在所有者只跑一条命令 `spawn mission push --source <来源> --commit <commit>`：它把 commit 放进来源的 `refs/spawn-owner/<hash>`（不动任何分支），记下 `source.push_requested`，打印这条请求的事件 id。集成器从不为此停下：每一轮在走任何任务组的步骤之前（第 0 步），按先后应用每条还没有结果记录的请求，持着落地锁（`spawn_landing_hold` 的锁，任务落地的推送与它的记录从不被所有者的落地隔开），从来源本身（不是镜像）读 `main`，快进或合并后用比较并交换移动 `main`，并在同一把锁里记下点名这条请求的 `source.moved`（基线随之移动）；冲突、来源里没有这个 commit、比较并交换连续失败两次，记 `source.push_refused`，什么都不动，也不停摆。所以本轮的基线检查从不会看到所有者移动过却没有记录的 `main`；一轮发现某来源还有未决的请求时（它的落地可能在记录之前就移动了 `main`），这个来源上按基线的停摆一律暂缓，直到请求被应用或拒绝；完全没有记录的移动（绕过 Spawn 的推送）照样停摆。别的任务组、别的来源的门禁照常跑完、照常落地；同一来源上正在跑的门禁跑完，结论照实记下，它落地时比较并交换发现 `main` 动过，按 `land()` 的既有规则记 `stale`，任务对着新的 `main` 重新合并、重新评判，不丢弃，也不计数。请求在前、点名它的结果在后，集成器只依据这两条记录行动，结果与哪个循环先跑无关。所有者的暂停（objective 34）只停任务的落地，不停所有者自己的：暂停中的源上的请求同样被应用，正如 `mission moved` 照常记下。

停滞的升级（`dispatch/stall.rs`，[fleet-architecture.md](fleet-architecture.md) §3 “Stalls”）同样守这条规则：每个未结任务所处的一步都从开始它的那条记录量起（控制记录的 id，或会话事实的 `session_seq`），年龄只用数据库的时钟（`spawn_now_ms()` 对 `recorded_at_ms`），从不凭“集成器最近没跑”之类的缺席推断；条目写明它量自哪条记录，标签里带着这条记录，所以每次停滞只升级一次，重启的 loop 算出同样的结果。

集成器的门禁按任务组并发（每个任务组、每个来源同一时刻至多一个判定，全局至多 `--gates` 个用共享测试库的判定，默认 1；场景目录里有 `no-test-database` 的任务组拿不到测试库地址，不占名额）。每个判定在自己的线程里跑完取分支、门禁与落地，各自记下 `task.gated`、`task.landed`；主循环不读线程的返回值，只读日志。结束的线程只在一轮开始、读任务组之前回收，所以一轮看到的任务组快照不会早于任何已结束判定的记录；每一轮都给每个任务组走完停机、预算、落地与目标分派，不因别的任务组的门禁而等；只有落地会让同一来源的其余任务组等下一轮（落地移动了 main 与它们的基线）。在途判定的来源不做镜像上的事（落地、拉取、判定）。空出的门禁给等得最久的就绪任务：从它报告时起，或从它所在任务组上一次判定结束时起，取较晚者；任务组编号不决定先后。进程重启时门禁随进程一起被杀，没有记录，下一个集成器重新判定。

门禁的主机故障同样守这条规则（objective 20，[fleet-architecture.md](fleet-architecture.md) §10.3）：check-tests.sh 在候选上失败时，集成器只依据记下的门禁输出里每条失败及其消息（记录的 `failures`），以及测试数据库日志里落在它自己量出的门禁时间窗内（前后各 1 秒）的崩溃行（记录的 `crash`），判定是主机的故障（`host_fault`）还是工作的（`rejected`），从不只凭拒绝理由的文字；之后的等待只依据记录里的 `due_ms`（记录自己的 `recorded_at_ms` 加退避，由存储写入），不凭进程内存或没有记录的时间。主机故障不计入任何停机规则，连续两次升级给 supervisor 一次。

门禁之前先量主机，不靠门禁崩溃才知道主机没有余量（objective 018）：每次判定在候选构建之后、门禁启动之前，集成器按序量门禁 `TMPDIR` 的剩余空间（`SPAWN_GATE_MIN_FREE_GB`）、`MemAvailable`（`SPAWN_GATE_MIN_FREE_MEMORY_MB`）与测试数据库（连接，再跑一条强制并行的查询，各 10 秒内），第一项不够就不启动门禁，记 `no_room` 并写明 `check`、`measured` 与 `threshold`。判断只凭判定当时的测量和它的阈值；一次跑了 25 分钟、因测试库 `/dev/shm` 不足而崩溃的门禁，不再被读成候选的回归。

写新代码时的检查：读另一个循环的产物时，问它覆盖的是哪一个 commit、哪一个会话位置、哪一个命令 id；读 `spawn_commands` 时区分成功、失败与尚未运行；超时只决定何时再看，不证明任何事；删掉一个生产者时，搜出它的每个读者。

## 15. 记分卡（`spawn scorecard`）

所有者的目标（2026-11-06）是 Spawn 在每个维度上同时优秀，处在帕累托前沿上：没有哪个维度靠另一个维度退步来变好。`spawn scorecard [--mission N] [--since <time>] [--until <time>] [--json] [--root <dir>]` 一次量出全部八个维度，只读记录：`session_control` 的控制记录（它们的 `recorded_at_ms`）、`spawn_commands` 的行，以及 `$SPAWN_WORKSPACE_ROOT/.transcripts`（或 `--root` 下的 `.transcripts`）里归档的 transcript。没有新增手工维护的计数器、表或列：读同一份记录的人得到同样的数，给出 `--until` 之后同样的参数得到逐字节相同的 JSON。看板的 Scorecard 视图（`web/src/components/scorecard/`）读 `GET /api/scorecard?mission=&since=&until=`，它回答的正是 `spawn scorecard --json` 同样参数的 JSON；`spawn serve` 以 reader 登录读库，从它自己的 `SPAWN_WORKSPACE_ROOT` 读 transcript。代码在 `crates/spawn-cli/src/scorecard.rs`（`history` 折叠记录，`measure` 计数，`transcripts` 读 token，`time` 读写时间），事实的查询在 `crates/spawn-postgres/src/scorecard.rs`。

**窗口。** `<time>` 是 RFC 3339（`2026-10-02T00:00:00Z`）或往回的一段时长（`36h`、`7d`）。`--until` 默认是现在（数据库的时钟 `spawn_now_ms()`，一段时长从现在往回数）；`--since` 默认 `7d`，一段时长从窗口的终点往回数，所以给定 `--until` 就固定了窗口。窗口是 [since, until)，至多 400 天；前一个窗口是紧挨在它前面、同样长的一段。不带 `--mission`：先是全队（fleet），然后按 id 列出每个在任一窗口里有任务在开、落地或有任何计入记录的任务（在 `until` 之前指派）的 mission；带 `--mission N`：只有那个 mission（没有记录提到它就拒绝）。事实在一个快照里读：`until` 之前的每条记录，之后的一条都不读。

**定义**（照目标原文）：

Times are a record's `recorded_at_ms`; an event belongs to a window or day when since <= t < until. Days are UTC dates; `days` lists every UTC day the window covers, oldest first, each computed as a window of its own (clipped to the window). A task is *landed* at its `task.landed`, or, on a mission without a gate, at its `task.reviewed` with `accepted` true; it is *closed* at its landing or its `task.cancelled`; it is *open* from its `task.assigned` until it is closed (or until the window's end). A task's mission is its `mission_id`. A command belongs to the mission of the task its key names (`deliver-<task>-...`), else of the task open on its session when it was created, else to the fleet only.

- **throughput**: landings in the window divided by the open hours, where the open hours are the length of the union of the scope's open-task intervals clipped to the window. Higher is better. Parts: `landed`, `open_hours`.
- **lead time**: for each landing in the window, its landing time minus its assignment time; for a task with an objective, the assignment of the earliest task of the same mission and objective that was cancelled at or before this task's assignment (the objective's first try, so a re-issue does not hide the wait). `value` is the median, `p90` the 90th percentile, both by nearest rank (the value at rank ceil(p/100 x n) of the sorted list), in whole ms. Lower is better.
- **first-pass quality**: the share of the window's landings whose task never had a send-back. Higher is better. Part: `first_pass`.
- **rework**: send-backs in the window per landing in the window. A send-back is a `task.gated` with verdict `rejected`, `conflict` or `incompatible`, or a `task.reviewed` with `accepted` false. Lower is better. Parts: `send_backs`, `by_verdict` {`rejected`, `conflict`, `incompatible`, `reviewed`}.
- **autonomy**: owner interventions in the window per landing. An intervention is (a) a command whose principal is `human` or `spawn` (the board's and the CLI's: Spawn's loops have principals of their own), at its creation; (b) a `mission.resumed` (only a human resumes); (c) an owner task: a `task.assigned` with `created_by` `human` on a mission whose `accept` is `review`; (d) a cancel-and-reassign: a `task.cancelled` whose `by` is absent or `human`, followed in its mission by a `task.assigned` for the same objective (or, without one, the same brief), counted at that assignment. A cancellation by one of Spawn's loops (`by` `spawn-dispatch`, objective 019) is not an intervention. Lower is better. Parts: `interventions`, `by_kind` {`command`, `resume`, `owner_task`, `cancel_reassign`}.
- **reliability**: failures of the machinery in the window per landing: every command finished with `succeeded` false (at its creation); every spent delivery, judged as deliver.rs `attempts()` judges it (no attempt succeeded, none open, none `MAY_HAVE_TYPED`, more than `DELIVERY_RETRIES` failures past the first `NOT_READY_FREE` refusals for a pane not ready), at its last attempt's finish; every stall, a distinct `stall-<task>-<step>-<n>` tag in a `spawn-dispatch` input's `tags`, counted once whatever the number of digests that carried it, at the first; and every infrastructure gate outcome, a `task.gated` with verdict `aborted`, `no_room` or `host_fault` (objective 020). Lower is better. Parts: `events`, `by_kind` {`failed_command`, `spent_delivery`, `stall`, `gate_infrastructure`}.
- **cost**: model tokens in the window per landing. A transcript's assistant line with a `message.usage` counts once per (agent, `message.id`), across every copy file, at its `timestamp`: input + cache creation + cache read + output tokens, under its `message.model`. A mission's tokens are those of its tasks' agents between each task's first `task.delivered` and its close (the window's end when open); the fleet's are every agent's, the supervisor's included. Also the gate's machine time: the sum of the gate runs' wall clock in the window (below). Lower is better. Parts: `tokens`, `by_model` {<model>: {`tokens`, `per_landed`}}, `gate_ms`, `gate_ms_per_landed`.
- **gate time**: the median (nearest rank) wall clock of the gate runs in the window, in ms. A gate run is a `task.gated` whose record names its wall clock: its `gate_ms` when present, else the last `| stage gate <result> <ms>ms total` line of its `log`; a record with neither ran no measured gate and is not counted. Lower is better. Part: `runs`.

**答案。** 每个维度，对每个范围、每一天，都是一个带 `value`、`n`、`enough`、`better`（`higher` 或 `lower`）、`previous`、`trend` 和它各部分的对象。`n` 是落地数（门禁耗时是门禁运行数）。最小 n 是 3（JSON 里的 `min_n`）：低于它，`value` 和每个按落地平均的部分（`p90`、`per_landed`、`gate_ms_per_landed`）都是 null，`enough` 为 false，文本形式写 `not enough data`（带 n 和最小值）而不写数。`previous` 是前一个窗口的值（那里不够时为 null）；`trend` 按维度的方向比较四舍五入后的值，给出 `better`、`worse` 或 `same`，任一为 null 时为 null；每一天的 `previous` 和 `trend` 都是 null。比值四舍五入到 3 位小数（一半时远离零，用整数算，不经浮点舍入）；毫秒和 token 数是整数。部分总在（没有值时为 null），从不省略。JSON 是 `{since_ms, until_ms, previous_since_ms, min_n, scopes: [{scope: "fleet"|"mission", mission: null|id, dimensions: {throughput, lead_time, first_pass, rework, autonomy, reliability, cost, gate_time}, days: [{day: "YYYY-MM-DD", dimensions: {...}}]}]}`；不带 `--json` 的文本形式给出同样的数，每个维度一行，带方向和趋势，再加每天一行的表。

**定义没说死的地方，这样读：**

- 一个任务只落地一次（第一条落地记录）；关闭之后的记录不再落地、不再关闭它。`task.aborted`（超预算中止，折叠成 `cancelled`）也是一次取消：它关闭任务，也算重试链里“取消过的一次尝试”，但它是集成器的，不是人的干预。
- 重新指派的前导时间：一个目标在本 mission 里上一次落地之前的尝试属于那次落地，不属于这一次（循环的目标每轮从自己的第一次尝试量起）。
- 一个会话上同时开着几个任务时（常驻工人的队列），命令归最早指派的那个（投递按最早的先送）。
- 取消再指派：每个人的取消，配它之后（按流的顺序）在同一 mission 里第一个同目标（或无目标时同 brief）的指派，各算一次。
- token：一个消息算在它最早一行的 `timestamp`，用它各行里最大的用量（实际上各行相同）；不是 assistant 的行、没有用量、没有消息 id 或时间读不出的行不算。最后写入早于前一个窗口起点一天以上的 transcript 文件不读：一行在它的消息写出时写入，这样的文件里没有窗口内的行（一天的余量吸收主机与数据库时钟的偏差）。一个消息同时落在几个 mission 的任务里时，每个 mission 各算一次，全队只算一次。
- 门禁日志的 stage 行在数据库里读出（只取整行 `| stage gate <result> <ms>ms total`，允许前后空白），日志本身不离开数据库；brief 只以 md5 比较。

**记录。** `task.cancelled` 写明 `by`（调用者）：所有者登录（CLI、人的关闭或退休）是 `human`，supervisor 是 `spawn_supervisor`，循环写自己的名字（集成器 `spawn-integrate`，调度器 `spawn-dispatch`，迁移补的记录 `spawn-migrate`）。更早的记录没有它，算人的取消；不回填。

**因果（§14）。** 每个数都读自一条写明自己覆盖了什么的记录：一条记录自己的时间和它点名的任务或 mission，一个命令的键、会话和时间，一行 transcript 的消息 id 和时间戳。没有任何东西凭缺席推断（任务直到一条记录关闭它之前都开着；投递只有它的尝试这么说时才算用尽，判断和 `deliver.rs` 的 `attempts()` 是同一段代码），也不凭循环运行的先后。
