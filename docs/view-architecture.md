# Spawn 视图层（读模型）设计

结论：视图分成 L0 到 L4 五层，每层写明粒度、主键、排序键与新鲜度；读权限有按 agent 过滤的授权函数；Rust 侧由 `PostgresStore` 直接提供读方法，不加端口。第 1 到 4 步已完成，第 5 步（投影表）未到门槛。

适用范围：`spawn-postgres` 的 SQL 视图、`spawn-core` 的读端口、`spawn-cli` 的查询子命令。写入路径与事件协议不在本文范围，见 [design.md](./design.md)。

## 1. 上下文链接

| 对象 | 位置 | 用途 |
| --- | --- | --- |
| 事件表与命令表 | [schema.sql:13](../crates/spawn-postgres/sql/schema.sql#L13)、[schema.sql:89](../crates/spawn-postgres/sql/schema.sql#L89) | 唯一事实源 |
| 写入函数 | [spawn_append:36](../crates/spawn-postgres/sql/schema.sql#L36)、[spawn_submit:117](../crates/spawn-postgres/sql/schema.sql#L117)、[spawn_claim:150](../crates/spawn-postgres/sql/schema.sql#L150)、[spawn_finish:184](../crates/spawn-postgres/sql/schema.sql#L184) | 生产事件与命令状态 |
| 授权表 | [spawn_grants:111](../crates/spawn-postgres/sql/schema.sql#L111)、[触发器:234](../crates/spawn-postgres/sql/schema.sql#L234) | 写入门禁的现有实现 |
| 端口定义 | [port.rs:39](../crates/spawn-core/src/port.rs#L39) 起 | `EventLog`、`CommandQueue`、`WorkspaceStore`、`TerminalHost` |
| 一致性套件 | [contract.rs:36](../crates/spawn-core/src/contract.rs#L36) 起 | 四个写端口的共用断言，内存实现与 PG 实现都跑 |
| 查询子命令 | [main.rs:224](../crates/spawn-cli/src/main.rs#L224) | `status` 目前分页扫描全部原始事件 |
| 门禁 | [remote-gate.sh](../scripts/remote-gate.sh)（同步后在 digest 上跑 [gate-on-digest.sh](../scripts/gate-on-digest.sh)）、[e2e-smoke.sh](../scripts/e2e-smoke.sh) | fmt、clippy、测试、黑盒 |
| 现有测试 | [spawn-postgres/tests/api.rs](../crates/spawn-postgres/tests/api.rs)、[spawn-core/tests/api.rs](../crates/spawn-core/tests/api.rs) | 断言落点 |
| 事件与 SQL 口径 | [design.md 第 9 节](./design.md) | 表、函数、视图的现有描述 |
| 架构总览 | [architecture.md 第 3、6 节](./architecture.md) | 运行链路与接口表 |

## 2. 起点（第 1 步之前）

7 个视图，全部由读时折叠生成，没有物化。下表是改动前的问题，第 1 到 4 步已处理，`agent_tasks` 的 prompt 与 `agent_artifacts` 的主键仍按原样。

| 视图 | 定义 | 问题 |
| --- | --- | --- |
| `agent_commands` | [230](../crates/spawn-postgres/sql/schema.sql#L230) | 只作写入口：public 只有 insert，没有 select |
| `agent_state` | [261](../crates/spawn-postgres/sql/schema.sql#L261) | 跨全部会话取 observation 再 `distinct on`；无 `event_id`，无法定位证据 |
| `agent_timeline` | [276](../crates/spawn-postgres/sql/schema.sql#L276) | 混入 Observer 与 Bridge 噪声；无分页键约束 |
| `agent_tasks` | [289](../crates/spawn-postgres/sql/schema.sql#L289) | `action` 只保留 type，prompt 丢失 |
| `agent_executions` | [304](../crates/spawn-postgres/sql/schema.sql#L304) | 无时长列。结束口径已修：只认 Bridge 的结束事件，runner 的退出报告进 `exit_reported` |
| `agent_artifacts` | [327](../crates/spawn-postgres/sql/schema.sql#L327) | 无复合主键，路由信息靠文本 |
| `agent_relations` | [341](../crates/spawn-postgres/sql/schema.sql#L341) | 只有一层父子，查不到祖先链 |

共同问题两条：

| 问题 | 证据 |
| --- | --- |
| 读权限不分 agent | [grant select:354](../crates/spawn-postgres/sql/schema.sql#L354) 把 6 个读视图授予 public，任一库用户能读全部会话；原始表已撤销 public 权限 |
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
| `agent_executions` | `execution_id` | `started_at_ms desc` | `ended_at_ms` 为空表示 Bridge 尚未结束该执行；`exit_reported` 表示引擎已退出、待下次 tick 清理 |
| `agent_artifacts` | `session_id` 加 `commit_id` | `session_seq` | 行内时间戳 |
| `agent_open_tools` | `session_id` 加 `tool_use_id` | `session_seq` | started 减 completed、failed，且所属执行未结束 |
| `agent_tasks` | `command_id` | `created_at_ms desc` | `finished_at_ms` |
| `agent_task_events` | 复合，命令加事件 | `session_seq` | 行内时间戳 |
| `agent_lineage` | `session_id` | 深度 | `session.linked` 折叠 |
| `agent_freshness` | `session_id` | — | `max(session_seq)`、`max(id)`、`last_event_at_ms`；不含判定滞后，见第 4.1 节 |

### 4.1 判定新鲜度

Observer 只在结论变化时写 `observation.recorded`。因此 `agent_state.based_on_seq < max(session_seq)` 不表示判定落后：Observer 可能已经判定过后续事件，结论不变所以没写。

| 能回答 | 不能回答 |
| --- | --- |
| 结论在哪个序号上最后一次变化（`based_on_seq`） | Observer 判定到了哪个序号 |
| 会话最后一个事件的时间（`agent_freshness`） | 当前结论是否已覆盖最新事件 |

需要回答右列时，让 Observer 每次 tick 写一条判定游标（例如每会话一行的 `spawn_observer_cursor` 表），不写事件，避免日志增长。在有消费方需要之前不加。

## 5. 必改五处

| 改动 | 具体做法 | 验收 |
| --- | --- | --- |
| `agent_state` 补证据 | 增加 `event_id`，与 `based_on_seq` 并列 | 断言同一 session 的 `event_id` 等于其 observation 事件 id |
| 新增 `agent_open_tools` | `tool.started` 按 `(session_id, tool_use_id)` 左连 `tool.completed`、`tool.failed`，取未闭合，排除执行已有 Bridge 结束事件的行 | 两个 started 一个 completed，结果恰一行；执行结束后结果为空 |
| 新增 `agent_freshness` | 每 `session_id` 取 `max(session_seq)`、`max(id)`、`max(recorded_at_ms)`、事件数 | 空会话不出现 |
| 修 `agent_executions`（已完成） | 结束行只取 `source = 'bridge'` 的 `execution.ended`；另加 `exit_reported`（存在引擎来源的结束事件） | runner 已报告退出、Bridge 未结束时 `ended_at_ms` 为空且 `exit_reported` 为真 |
| 拆时间线 | `agent_timeline` 排除 `source = 'observer'`，判定改走 `agent_judgements` | 时间线不含 observation，判定视图只含 observation |

## 6. 权限

| 方案 | 做法 | 取舍 |
| --- | --- | --- |
| A 单租户（现状） | 视图公开；原始表与 `agent_commands` 的 select 已收回 | 无需改动；库用户能读全部会话 |
| B 多租户 | 视图收回 public，客户端走 `spawn_read_*` 函数，内部按 `spawn_grants` 过滤 `session_user` | 权限一致；客户端不能直查视图，必须走函数 |

函数门面已上线，与公开视图并行：`spawn_read_state()`、`spawn_read_tasks()`、`spawn_read_relations()`、`spawn_read_freshness()` 跨会话；`spawn_read_timeline`、`judgements`、`executions`、`artifacts`、`task_events`、`lineage`、`open_tools` 取 `session_id` 参数。切到 B 只需 `revoke select on agent_* from public`。

多租户时选 B。写入路径已经在 [触发器:234](../crates/spawn-postgres/sql/schema.sql#L234) 校验 `spawn_grants`，读取路径不校验。单租户部署保持 A。读函数与触发器一样是 `security definer`，必须同样固定 `search_path`。

实施 B 会断开现有直查视图的客户端。上线前先确认外部消费方，或先并行提供函数门面再收回 public。

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

落点：

| 文件 | 改动 |
| --- | --- |
| `spawn-postgres/src/lib.rs` | 增上表的读方法，查询走视图，错误用 `StoreError` |
| [spawn-postgres/tests/api.rs](../crates/spawn-postgres/tests/api.rs) | 每个方法一条断言 |
| [main.rs:224](../crates/spawn-cli/src/main.rs#L224) | `cmd_status` 改调 `states()`，不再扫描原始事件 |

出现第二个读实现（例如 HTTP 读服务）时，再把这些方法提成 trait。

## 8. 验证

| 手段 | 内容 | 落点 |
| --- | --- | --- |
| 视图断言 | 脚本化事件序列后断言每视图行集、排序、去重、空值 | `spawn-postgres/tests/api.rs` |
| 列集快照 | 查 `information_schema.columns`，锁定列名与类型 | `spawn-postgres/tests/api.rs` |
| 四态覆盖 | `spawn_finish` 的 `done`、`already`、`stale`、`missing` 各一条 | 同上 |
| 授权用例 | 无 grant 读函数报 `42501`，有 grant 只返回本 agent 的行 | 同上 |
| 计划检查 | 万级事件上 `explain` `agent_state`。现有索引是 `(session_id, event_name, id)`，跨会话查询用不上前缀；按需加部分索引 `on (session_id, session_seq desc) where event_name = 'observation.recorded'` | 门禁一次性检查 |
| 全量门禁 | `bash scripts/remote-gate.sh` | 每轮必跑 |

现有覆盖：全部视图与六个读方法、`spawn_finish` 四态、`agent_commands` 无授权被拒与有授权写入、读函数无授权为空与按 agent 过滤。缺口：列集快照、万级事件 `explain`。

## 9. 演进顺序

| 步 | 内容 | 完成条件 |
| --- | --- | --- |
| 1（已完成） | 修 `agent_executions` 结束口径，补现有视图断言 | runner 先报告退出、Bridge 后结束的序列有断言 |
| 2（已完成） | `PostgresStore` 读方法，加 `agent_open_tools`、`agent_freshness`、`agent_state.event_id` | 新视图有断言，`status` 切到 `states()` |
| 3（已完成） | 加 `agent_lineage`、`agent_judgements`、拆时间线、`agent_task_events`，补 `duration_ms` | 深链、命令关联各有断言 |
| 4（函数已上线） | 上 `security definer` 读函数；收回视图 public 待外部消费方迁移 | 授权用例通过 |
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
| 来源 | `spawn_events.source` 为小写：`claude`、`codex`、`bridge`、`layerfs`、`observer`、`gateway` |
| 执行结束 | 以 `source = 'bridge'` 的 `execution.ended` 为准；`claude`、`codex` 来源的同名事件是 runner 的退出报告 |
| 去重 | `(session_id, source, source_event_id)` 唯一；`execution.started` 键为 `<exec>:started`，同一执行不会出现两条 |
| 分页 | keyset，按视图排序键，不用 offset |
| 视图角色 | 只读；写入口是 `agent_commands` 触发器与四个函数 |
| 命名 | 视图统一 `agent_*`，函数前缀 `spawn_` |
