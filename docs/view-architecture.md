# Spawn 视图层（读模型）设计

结论：视图分成 L0 到 L4 五层，每层写明粒度、主键、排序键与新鲜度；视图只归 owner，其他角色走按 agent 过滤的 `spawn_read_*` 函数；`agent_state`、`agent_status`、`agent_freshness` 读每会话一行的投影表 `spawn_session_state`；Rust 侧由 `PostgresStore` 直接提供读方法，不加端口。

适用范围：`spawn-postgres` 的 SQL 视图、`spawn-core` 的读端口、`spawn-cli` 的查询子命令。写入路径与事件协议不在本文范围，见 [design.md](./design.md)。

## 1. 上下文链接

| 对象 | 位置 | 用途 |
| --- | --- | --- |
| 事件表与命令表 | [schema.sql](../crates/spawn-postgres/sql/schema.sql) 的 `spawn_events`、`spawn_commands` | 唯一事实源 |
| 写入函数 | 同上，`spawn_append`、`spawn_submit`、`spawn_claim`、`spawn_finish` | 生产事件与命令状态 |
| 授权表 | 同上，`spawn_grants` 与触发器 `spawn_gateway_insert` | 写入门禁 |
| 投影与视图 | 同上，`spawn_session_state`、`agent_*`、`spawn_read_*` | 读模型 |
| 端口定义 | [port.rs](../crates/spawn-core/src/port.rs) | `EventLog`、`CommandQueue`、`WorkspaceStore`、`ChunkStore`、`TerminalHost` |
| 一致性套件 | [contract.rs](../crates/spawn-core/src/contract.rs) | 四个端口（日志、队列、工作区、终端）的共用断言，内存实现与 PG 实现都跑 |
| 查询子命令 | [inspect.rs](../crates/spawn-cli/src/commands/inspect.rs) | `status` 调 `states()` 读 `agent_state` |
| 门禁 | [gate.sh](../scripts/rsi/gate.sh) 与 [rsi/scenarios](../rsi/scenarios) | 构建、clippy、测试、e2e、黑盒、web |
| 测试 | [spawn-postgres/tests/api.rs](../crates/spawn-postgres/tests/api.rs)、[spawn-core/tests/api.rs](../crates/spawn-core/tests/api.rs) | 断言落点 |
| 事件与 SQL 口径 | [design.md 第 9 节](./design.md) | 表、函数、视图的描述 |
| 架构总览 | [architecture.md 第 3、6 节](./architecture.md) | 运行链路与接口表 |

所有视图都在读时由日志生成或读投影表，没有物化视图。

## 2. 视图的公共约束

| 约束 | 做法 |
| --- | --- |
| 读权限按 agent | 视图只归 owner；其他角色走 `spawn_read_*` 函数，按 `spawn_grants` 过滤（第 6 节） |
| 滞后可见 | `agent_state` 带 `event_id` 与 `based_on_seq`，`agent_freshness` 给每会话最后位置与时间（第 4 节） |

## 3. 分层

| 层 | 视图 | 粒度 | 回答的问题 |
| --- | --- | --- | --- |
| L0 事实 | `spawn_events`、`spawn_commands` | 一事件、一命令 | 发生了什么 |
| L1 折叠 | `agent_state`、`agent_status`（判定加最近一次执行） | 一会话一行，读投影表 | 现在什么状态 |
| L2 明细 | `agent_timeline`、`agent_judgements`、`agent_executions`、`agent_artifacts`、`agent_open_tools` | 一事件、一次执行、一次提交、一个未闭合工具 | 为什么是这个状态 |
| L3 结构 | `agent_tasks`、`agent_task_events`、`agent_lineage` | 一命令、命令与事件关联、一条祖先链 | 任务怎么走完、会话属于谁 |
| L4 元 | `agent_freshness` | 一会话一行 | 读到的是不是最新 |

## 4. 视图契约

每个视图必须写清四件事：粒度、主键、排序键、新鲜度。客户端按排序键做 keyset 分页，禁止 offset。

| 视图 | 主键 | 排序键 | 新鲜度 |
| --- | --- | --- | --- |
| `agent_state` | `session_id` | `session_seq desc` | `event_id`、`based_on_seq` |
| `agent_status` | `session_id` | — | 投影表的判定与位置，加最近一次执行；`live` 表示该执行尚未被 Bridge 结束 |
| `agent_timeline` | `event_id` | `session_seq`、`id` | 行内 `recorded_at_ms` |
| `agent_judgements` | `event_id` | `session_seq` | 行内 `recorded_at_ms` |
| `agent_executions` | `execution_id` | `started_at_ms desc` | `ended_at_ms` 为空表示 Bridge 尚未结束该执行；`exit_reported` 表示引擎已退出、待下次 tick 清理 |
| `agent_artifacts` | `session_id` 加 `commit_id` | `session_seq` | 行内时间戳 |
| `agent_open_tools` | `session_id` 加 `tool_use_id` | `session_seq` | started 减 completed、failed，且所属执行未结束 |
| `agent_tasks` | `command_id` | `created_at_ms desc` | `finished_at_ms`；`prompt` 取 action 的 `prompt` 或 `text` |
| `agent_task_events` | 复合，命令加事件 | `session_seq` | 行内时间戳 |
| `agent_lineage` | `session_id` | 深度 | `session.linked` 折叠 |
| `agent_freshness` | `session_id` | — | `max(session_seq)`、`max(id)`、`last_event_at_ms`；不含判定滞后，见第 4.1 节 |

### 4.1 判定新鲜度

规则的结论只在变化时写 `observation.recorded`（判定器的回答每次都写）。因此 `agent_state.based_on_seq < max(session_seq)` 不表示判定落后：Observer 可能已经判定过后续事件，结论不变所以没写。

| 能回答 | 不能回答 |
| --- | --- |
| 结论在哪个序号上最后一次变化（`based_on_seq`） | Observer 判定到了哪个序号 |
| 会话最后一个事件的时间（`agent_freshness`） | 当前结论是否已覆盖最新事件 |

需要回答右列时，让 Observer 每次 tick 写一条判定游标（例如每会话一行的 `spawn_observer_cursor` 表），不写事件，避免日志增长。在有消费方需要之前不加。

## 5. 视图定义要点

| 视图 | 定义 | 断言 |
| --- | --- | --- |
| `agent_state` | 带 `event_id`，与 `based_on_seq` 并列 | 同一 session 的 `event_id` 等于其 observation 事件 id |
| `agent_open_tools` | `tool.started` 按 `(session_id, tool_use_id)` 左连 `tool.completed`、`tool.failed`，取未闭合，排除执行已有 Bridge 结束事件的行 | 两个 started 一个 completed，结果恰一行；执行结束后结果为空 |
| `agent_freshness` | 每 `session_id` 取 `max(session_seq)`、`max(id)`、`max(recorded_at_ms)`、事件数 | 空会话不出现 |
| `agent_executions` | 结束行只取 `source = 'bridge'` 的 `execution.ended`；`exit_reported` 表示存在其他来源的结束事件 | 已报告退出、Bridge 未结束时 `ended_at_ms` 为空且 `exit_reported` 为真 |
| `agent_timeline` / `agent_judgements` | 时间线排除 `source = 'observer'`，判定走 `agent_judgements` | 时间线不含 observation，判定视图只含 observation |

## 6. 权限

采用多租户方案：原始表、投影表与全部 `agent_*` 视图都收回 public，只留 `agent_commands` 的 insert（和 `id` 列的 select，供 `returning id` 用）。其他角色走函数：`spawn_read_state()`、`spawn_read_status()`、`spawn_read_tasks()`、`spawn_read_freshness()` 跨会话；`spawn_read_timeline(session_id)`、`spawn_read_judgements(session_id)` 取会话参数。其余视图没有读函数。`spawn_reader`（`spawn serve` 的登录）例外：它能 select 全部表和视图，什么都不能写。函数是 `security definer`，固定 `search_path`，按 `spawn_grants` 过滤 `session_user`，`'*'` 表示全部 agent。

`spawn` CLI 以 owner 连接，直读视图。迁移对视图执行 revoke；直查视图的非 owner 客户端收到 `42501`，应改走函数。

## 7. Rust 读路径

读方法直接加在 `PostgresStore` 上，不在 `spawn-core` 加端口。视图只有 SQL 一种实现；为内存实现再写一遍折叠，等于第二份口径，与第 10 节的漂移风险相反。Bridge 与 Observer 不读视图，所以 core 不需要这个端口。

| 方法 | 对应视图 | 用途 |
| --- | --- | --- |
| `states()` | `agent_state` | 全部会话当前状态，供 `spawn status` |
| `state(&SessionId)` | `agent_state` | 单会话当前状态 |
| `timeline(&SessionId, after, limit)` | `agent_timeline` | keyset 分页读事件 |
| `executions(&SessionId)` | `agent_executions` | 执行历史与时长 |
| `open_tools(&SessionId)` | `agent_open_tools` | 未闭合工具 |
| `freshness(&SessionId)` | `agent_freshness` | 判断是否落后 |

读方法的查询走视图，错误用 `StoreError`；[spawn-postgres/tests/api.rs](../crates/spawn-postgres/tests/api.rs) 对每个方法有断言；`spawn status` 调 `states()`。

出现第二个读实现（例如不经 PostgreSQL 的读存储）时，再把这些方法提成 trait。

## 8. 验证

| 手段 | 内容 | 落点 |
| --- | --- | --- |
| 视图断言 | 脚本化事件序列后断言每视图行集、排序、去重、空值 | `spawn-postgres/tests/api.rs` |
| 列集快照 | 查 `information_schema.columns`，锁定列名与类型 | `spawn-postgres/tests/api.rs` |
| 四态覆盖 | `spawn_finish` 的 `done`、`already`、`stale`、`missing` 各一条 | 同上 |
| 授权用例 | 无 grant 读函数报 `42501`，有 grant 只返回本 agent 的行 | 同上 |
| 计划检查 | 万级事件上 `explain` 各视图。索引是 `(session_id, event_name, id)`，跨会话查询用不上前缀；需要时加部分索引 `on (session_id, session_seq desc) where event_name = 'observation.recorded'` | 手动检查 |
| 全量门禁 | `scripts/rsi/gate.sh "$PWD/.git" HEAD HEAD "$PWD/rsi/scenarios"` | 每次落地必跑 |

覆盖：全部视图与六个读方法、列集快照、投影与日志折叠逐行相等（含守卫与回填）、`spawn_finish` 四态、`agent_commands` 无授权被拒与有授权写入、非 owner 直查视图报 `42501`、读函数无授权为空与按 agent 过滤。

计划检查结果（PG 16，20 会话共 2 万事件，状态类视图读投影表）：`agent_state` 读投影 0.05 ms；`agent_status` 1.1 ms；单会话 `agent_timeline` 分页 0.1 ms、`agent_judgements` 0.1 ms、`agent_executions` 0.2 ms、`agent_open_tools` 0.4 ms、`agent_task_events` 1.3 ms，都走已有索引；`spawn_append` 含投影更新 2.2 ms。这个规模下不需要部分索引。

## 9. 投影

投影只存每会话一行（位置、事件数、最新判定），每次 append 多写一行。日志仍是唯一真值：行只在恰好落后一个事件时步进，否则 `spawn_session_fold` 从日志重折；投影可删除，迁移时持 append 锁重折。会话的 `agent_id` 取首个事件的 agent，其他 agent 经网关写入的事件不改变归属，`agent_state.agent_id` 同此口径。

## 10. 风险

| 风险 | 影响 | 处置 |
| --- | --- | --- |
| 收权限断开客户端 | 直查视图的非 owner 客户端报 `42501` | 改走 `spawn_read_*` 函数 |
| 视图改列 | 客户端字段读取失败 | 列集快照测试挡住静默改动 |
| 投影与日志不一致 | `agent_state` 给出错误判定 | 只在 `spawn_append` 内写投影；测试断言投影等于折叠；删除投影行后迁移回填 |
| 视图与事件口径漂移 | 同一事实两处解释 | 每个视图只允许一种折叠，断言锁口径 |
| 迁移回填变慢 | 每次 `init-db` 按会话聚合全量日志 | 只重折与日志不一致的会话；日志很大时加迁移版本号，只跑一次 |
| 迁移与 append 并发 | 替换前的函数体写的事件不进投影 | 迁移先取 append 锁，等在途事务提交后再重折 |

## 11. 附录：口径

| 项 | 口径 |
| --- | --- |
| 事件名 | `spawn_events.event_name` 生成列，取 `kind->>'name'` |
| 序号 | `session_seq` 单会话稠密，从 1 开始；`id` 全局 |
| 时间 | `bigint` 毫秒，统一取 `spawn_now_ms()` |
| 来源 | `spawn_events.source` 为小写：`claude`、`bridge`、`observer`、`gateway`、`control` |
| 执行结束 | 以 `source = 'bridge'` 的 `execution.ended` 为准；`claude` 来源的同名事件是退出报告 |
| 去重 | `(session_id, source, source_event_id)` 唯一；`execution.started` 键为 `<exec>:started`，同一执行不会出现两条 |
| 分页 | keyset，按视图排序键，不用 offset |
| 视图角色 | 只读；写入口是 `agent_commands` 触发器与四个函数 |
| 命名 | 视图统一 `agent_*`，函数前缀 `spawn_` |
