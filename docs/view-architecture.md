# Spawn 视图层（读模型）设计

结论：视图分成 L0 到 L4 五层，每层写明粒度、主键、排序键与新鲜度；读权限从公开视图收成授权函数；Rust 侧补 `ReadStore` 端口。现有 7 个视图是平铺的，缺契约、缺滞后标识，读路径也没有授权。

适用范围：`spawn-postgres` 的 SQL 视图、`spawn-core` 的读端口、`spawn-cli` 的查询子命令。写入路径与事件协议不在本文范围，见 [design.md](./design.md)。

## 1. 上下文链接

| 对象 | 位置 | 用途 |
| --- | --- | --- |
| 事件表与命令表 | [schema.sql:13](../crates/spawn-postgres/sql/schema.sql#L13)、[schema.sql:89](../crates/spawn-postgres/sql/schema.sql#L89) | 唯一事实源 |
| 写入函数 | [spawn_append:36](../crates/spawn-postgres/sql/schema.sql#L36)、[spawn_submit:117](../crates/spawn-postgres/sql/schema.sql#L117)、[spawn_claim:150](../crates/spawn-postgres/sql/schema.sql#L150)、[spawn_finish:184](../crates/spawn-postgres/sql/schema.sql#L184) | 生产事件与命令状态 |
| 授权表 | [spawn_grants:111](../crates/spawn-postgres/sql/schema.sql#L111)、[触发器:234](../crates/spawn-postgres/sql/schema.sql#L234) | 写入门禁的现有实现 |
| 端口定义 | [port.rs:39](../crates/spawn-core/src/port.rs#L39) 起 | `EventLog`、`CommandQueue`、`WorkspaceStore`、`TerminalHost` |
| 一致性套件 | [contract.rs:36](../crates/spawn-core/src/contract.rs#L36) 起 | 四个实现层共用断言 |
| 查询子命令 | [main.rs:218](../crates/spawn-cli/src/main.rs#L218) | `status` 目前读原始事件拼装 |
| 门禁 | [gate-on-digest.sh](../scripts/gate-on-digest.sh)、[e2e-smoke.sh](../scripts/e2e-smoke.sh) | fmt、clippy、测试、黑盒 |
| 现有测试 | [spawn-postgres/tests/api.rs](../crates/spawn-postgres/tests/api.rs)、[spawn-core/tests/api.rs](../crates/spawn-core/tests/api.rs) | 断言落点 |
| 事件与 SQL 口径 | [design.md 第 9 节](./design.md) | 表、函数、视图的现有描述 |
| 架构总览 | [architecture.md 第 3、6 节](./architecture.md) | 运行链路与接口表 |

## 2. 现状

7 个视图，全部由读时折叠生成，没有物化。

| 视图 | 定义 | 问题 |
| --- | --- | --- |
| `agent_commands` | [230](../crates/spawn-postgres/sql/schema.sql#L230) | 把 `idempotency_key` 与原始 action 暴露给 public |
| `agent_state` | [261](../crates/spawn-postgres/sql/schema.sql#L261) | 全表扫 observation 再 `distinct on`；无 `event_id`，无法定位证据 |
| `agent_timeline` | [276](../crates/spawn-postgres/sql/schema.sql#L276) | 混入 Observer 与 Bridge 噪声；无分页键约束 |
| `agent_tasks` | [289](../crates/spawn-postgres/sql/schema.sql#L289) | `action` 只保留 type，prompt 丢失 |
| `agent_executions` | [304](../crates/spawn-postgres/sql/schema.sql#L304) | 未按 `execution_id` 去重；无时长列 |
| `agent_artifacts` | [327](../crates/spawn-postgres/sql/schema.sql#L327) | 无复合主键，路由信息靠文本 |
| `agent_relations` | [341](../crates/spawn-postgres/sql/schema.sql#L341) | 只有一层父子，查不到祖先链 |

共同问题两条：

| 问题 | 证据 |
| --- | --- |
| 读权限全开 | [grant select:352](../crates/spawn-postgres/sql/schema.sql#L352) 把 `spawn_events` 与全部视图授予 public |
| 无滞后标识 | 任一视图都不带 `as_of`，消费方无法判断落后多少 |

## 3. 目标分层

| 层 | 视图 | 粒度 | 回答的问题 |
| --- | --- | --- | --- |
| L0 事实 | `spawn_events`、`spawn_commands` | 一事件、一命令 | 发生了什么 |
| L1 折叠 | `agent_state`、`agent_status` | 一会话一行 | 现在什么状态 |
| L2 明细 | `agent_timeline`、`agent_judgements`、`agent_executions`、`agent_artifacts`、`agent_open_tools` | 一事件、一次执行、一次提交、一个未闭合工具 | 为什么是这个状态 |
| L3 结构 | `agent_tasks`、`agent_task_events`、`agent_lineage` | 一命令、命令与事件关联、一条祖先链 | 任务怎么走完、会话属于谁 |
| L4 元 | `agent_freshness` | 一会话一行 | 读到的是不是最新 |

## 4. 视图契约

每个视图必须写清四件事：粒度、主键、排序键、新鲜度。客户端按排序键做 keyset 分页，禁止 offset。

| 视图 | 主键 | 排序键 | 新鲜度 |
| --- | --- | --- | --- |
| `agent_state` | `session_id` | `session_seq desc` | `event_id`、`based_on_seq` |
| `agent_status` | `session_id` | — | 由 `agent_state` 与最近执行合成 |
| `agent_timeline` | `event_id` | `session_seq`、`id` | 行内 `recorded_at_ms` |
| `agent_judgements` | `event_id` | `session_seq` | 行内 `recorded_at_ms` |
| `agent_executions` | `execution_id` | `started_at_ms desc` | `ended_at_ms` 为空表示在跑 |
| `agent_artifacts` | `session_id` 加 `commit_id` | `session_seq` | 行内时间戳 |
| `agent_open_tools` | `tool_use_id` | `session_seq` | started 减 completed、failed |
| `agent_tasks` | `command_id` | `created_at_ms desc` | `finished_at_ms` |
| `agent_task_events` | 复合，命令加事件 | `session_seq` | 行内时间戳 |
| `agent_lineage` | `session_id` | 深度 | `session.linked` 折叠 |
| `agent_freshness` | `session_id` | — | `max(session_seq)`、`max(id)`、`last_event_at_ms` |

## 5. 必改五处

| 改动 | 具体做法 | 验收 |
| --- | --- | --- |
| `agent_state` 补证据 | 增加 `event_id`，与 `based_on_seq` 并列 | 断言同一 session 的 `event_id` 等于其 observation 事件 id |
| 新增 `agent_open_tools` | `tool.started` 左连 `tool.completed`、`tool.failed`，取未闭合 | 两个 started 一个 completed，结果恰一行 |
| 新增 `agent_freshness` | 每 `session_id` 取 `max(session_seq)`、`max(id)`、`max(recorded_at_ms)`、事件数 | 空会话不出现 |
| 修 `agent_executions` | `distinct on (execution_id)`，补 `duration_ms` | 重复 started 只出一行 |
| 拆时间线 | `agent_timeline` 排除 `source = 'Observer'`，判定改走 `agent_judgements` | 时间线不含 observation，判定视图只含 observation |

## 6. 权限

| 方案 | 做法 | 取舍 |
| --- | --- | --- |
| A 单租户 | 保留视图公开，收回 `spawn_events` 与 `agent_commands` 的 select | 改动小；库用户能读全部会话 |
| B 多租户 | 视图收回 public，暴露 `spawn_read_*(principal)` 函数，内部按 `spawn_grants` 过滤 | 权限一致；客户端不能直查视图，必须走函数 |

推荐 B。写入路径已经在 [触发器:234](../crates/spawn-postgres/sql/schema.sql#L234) 校验 `spawn_grants`，读取路径没有任何校验，这是直接缺口。

实施 B 会断开现有直查视图的客户端。上线前先确认外部消费方，或先并行提供函数门面再收回 public。

## 7. Rust 读路径

在 `spawn-core` 增 `ReadStore` 端口，实现放 `spawn-postgres`。

| 方法 | 对应视图 | 用途 |
| --- | --- | --- |
| `state(&SessionId)` | `agent_state` | 单会话当前状态 |
| `timeline(&SessionId, after, limit)` | `agent_timeline` | keyset 分页读事件 |
| `executions(&SessionId)` | `agent_executions` | 执行历史与时长 |
| `open_tools(&SessionId)` | `agent_open_tools` | 未闭合工具 |
| `freshness(&SessionId)` | `agent_freshness` | 判断是否落后 |

落点：

| 文件 | 改动 |
| --- | --- |
| [port.rs](../crates/spawn-core/src/port.rs) | 增 `ReadStore` trait 与 `ReadError` |
| [contract.rs](../crates/spawn-core/src/contract.rs) | 增 `read_store(tag, store, log)` 套件 |
| `spawn-postgres/src/lib.rs` | 实现 `ReadStore`，查询走视图 |
| [main.rs:218](../crates/spawn-cli/src/main.rs#L218) | `cmd_status` 改调 `ReadStore::state`，不再拼原始事件 |

## 8. 验证

| 手段 | 内容 | 落点 |
| --- | --- | --- |
| 契约套件 | 脚本化事件序列后断言每视图行集、排序、去重、空值 | `spawn-core/src/contract.rs` |
| 列集快照 | 查 `information_schema.columns`，锁定列名与类型 | `spawn-postgres/tests/api.rs` |
| 四态覆盖 | `spawn_finish` 的 `done`、`already`、`stale`、`missing` 各一条 | 同上 |
| 授权用例 | 无 grant 读函数报 `42501`，有 grant 只返回本 agent 的行 | 同上 |
| 计划检查 | 万级事件上 `explain` `agent_state`，确认走 `(event_name, session_seq)` 索引 | 门禁一次性检查 |
| 全量门禁 | `bash scripts/gate-on-digest.sh` | 每轮必跑 |

现有覆盖缺口：`agent_executions`、`agent_artifacts`、`agent_relations`、`agent_commands` 与 `spawn_finish` 四态没有断言。上述契约套件落地后一并补齐。

## 9. 演进顺序

| 步 | 内容 | 完成条件 |
| --- | --- | --- |
| 1 | 补 `ReadStore` 与 `read_store` 契约套件 | 内存实现与真 PG 都过同一套断言 |
| 2 | 加 `agent_open_tools`、`agent_freshness`、`agent_state.event_id` | 新视图有断言，`status` 切到 `ReadStore` |
| 3 | 修 `agent_executions` 去重与时长，加 `agent_lineage`、`agent_task_events` | 重复 started、深链、命令关联各有断言 |
| 4 | 收权限，上 `security definer` 读函数 | 授权用例通过，外部消费方已迁移 |
| 5 | 单会话事件过万后再上投影表 | `spawn_append` 内更新 `spawn_session_state`，带 `session_seq` 守卫 |

第 5 步之前不物化。当前读代价是 O(会话事件数)，量级够用；提前物化会带来写放大与第二份真值。

## 10. 风险

| 风险 | 影响 | 处置 |
| --- | --- | --- |
| 收权限断开客户端 | 现有 SQL 消费方直接报错 | 先并行提供函数门面，再收 public |
| 视图改列 | 客户端字段读取失败 | 列集快照测试挡住静默改动 |
| 读时折叠变慢 | 单会话事件过万后 `distinct on` 变贵 | 加部分索引，过万再上投影表 |
| 视图与事件口径漂移 | 同一事实两处解释 | 每个视图只允许一种折叠，断言锁口径 |
| 物化过早 | 写放大、双真值 | 第 5 步设门槛，按会话事件量触发 |

## 11. 附录：口径

| 项 | 口径 |
| --- | --- |
| 事件名 | `spawn_events.event_name` 生成列，取 `kind->>'name'` |
| 序号 | `session_seq` 单会话稠密，从 1 开始；`id` 全局 |
| 时间 | `bigint` 毫秒，统一取 `spawn_now_ms()` |
| 分页 | keyset，按视图排序键，不用 offset |
| 视图角色 | 只读；写入口是 `agent_commands` 触发器与四个函数 |
| 命名 | 视图统一 `agent_*`，函数前缀 `spawn_` |
