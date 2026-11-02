# Spawn
# Persistent Agent Runtime

核心方案：使用现成的 Agent CLI 执行计算、LayerFS 管理工作区、PostgreSQL 保存事件、Jev 判断状态，最终通过 SQL 向人和其他 Agent 提供统一接口。

不重新开发 Agent Framework。只实现少量适配器，将五个模块连接起来。

## 一、整体架构

5\. Consumer（人 / 上层 Agent）

查询状态 · 订阅变化 · 下发任务

SQL / Subscribe / Command

4\. SQL Layer

SQL Gateway · Materialized Views · CDC

查询与变更流

Storage（统一存储边界）

所有历史、文件、事件的事实来源

1\. LayerFS

Workspace · File

Branch · Commit

2\. Event Log

Agent · Session

Execution · Observation

Commit ID ↔ Event ID ↔ Session ID

3\. Compute

Claude / Codex

tmux + Agent Bridge

读写 Storage

4\. Observer

1Hz Heartbeat

Jev Adapter

读取事件和 Storage

图中的编号对应五种组件能力；Storage 包含 LayerFS 和 Event Log。Compute 与 Observer 只分别对接 Storage，不直接互相调用。

## 二、模块之间到底用什么连接？

| 连接关系                | 接口技术                                 | 具体用途                   |
| ------------------- | ------------------------------------ | ---------------------- |
| Claude/Codex ↔ tmux | PTY                                  | 启动、输入、输出、终端交互          |
| Agent ↔ Event Log   | Hooks / JSONL / Bridge               | 捕获对话、工具和执行事件           |
| Agent ↔ LayerFS     | POSIX / FUSE                         | 读取、修改文件                |
| Bridge ↔ LayerFS    | LayerFS SDK                          | Commit、Branch、Merge、恢复 |
| Jev ↔ Storage       | SQL 读取 + 事件游标                        | 每秒判断当前状态               |
| LayerFS ↔ Event Log | Commit ID + 关联事件                     | 将文件版本与执行历史关联           |
| SQL ↔ Storage       | PostgreSQL CDC + LayerFS Backend API | 自动构建状态和产物视图            |
| Consumer ↔ SQL      | SQL / 流式订阅                           | 查询、订阅和提交命令             |

这里最重要的是两条链路：

- 数据链路：Agent → Storage → SQL → Consumer。
- 状态链路：Storage → Observer → Storage → SQL → Consumer。

两条链路互不依赖。

## 三、统一身份：首先解决 Session 映射

一个 Agent 可以长期存在，但 Claude、Codex、tmux 和 LayerFS 各有自己的 ID。

因此需要一个统一映射表。

| 系统对象           | ID 示例               | 含义           |
| -------------- | ------------------- | ------------ |
| Agent          | `agent_001`         | 长期存在的逻辑身份    |
| Session        | `session_001`       | Agent 的连续历史  |
| Execution      | `exec_045`          | 本次计算         |
| tmux           | `tmux_agent_001`    | 当前终端会话       |
| Claude/Codex   | `native_session_id` | 模型 CLI 的原生会话 |
| LayerFS        | `branch_agent_001`  | 文件历史分支       |
| LayerFS Commit | `commit_id`         | 某次文件状态       |
| Event          | `event_id`          | 一条不可变事实      |

这里有一个重要设计：

Agent Session 不等于 Claude Session，也不等于 tmux Session。

例如：

```
Agent: agent_001
  Session: session_001（持续存在）
    ├── Claude Session A
    │     ├── Execution 1
    │     └── Execution 2
    ├── Claude Session B（恢复或重建）
    │     └── Execution 3
    └── Codex Session C（切换执行引擎）
          └── Execution 4

  LayerFS Branch: branch_agent_001
```

即使换了模型、终端或者机器，Agent 的逻辑 Session 仍然存在。

这项映射由 Agent Bridge 创建，并持久化在 Event Log 中。

## 四、Agent Bridge：计算与存储的连接器

这是整个方案里最重要的自研组件，但可以做得很薄。

### 4.1 职责

Agent Bridge 只做四件事：

| 能力         | 接口            | 作用              |
| ---------- | ------------- | --------------- |
| 启动与恢复      | tmux / 原生 CLI | 启动 Claude、Codex |
| 事件采集       | Hooks / JSONL | 获取真实执行记录        |
| 文件提交       | LayerFS SDK   | 保存文件修改          |
| Session 映射 | Event Log     | 关联所有原生 ID       |

不实现 Agent 的推理循环，不接管模型决策。

### 4.2 Claude 的接入

```
Agent Bridge
    │
    ├── tmux + Claude Code
    │       └── PTY：输入输出
    │
    ├── Claude Hooks
    │       └── 结构化事件
    │
    ├── Claude Transcript
    │       └── 对话与工具历史
    │
    └── LayerFS Workspace
            └── 文件读写
```

可以先采用：

```
tmux new-session -d -s agent_001 \
  'claude --ax-screen-reader'
```

Claude Code 官方支持 `--ax-screen-reader`，也支持通过 `claude -r "<session-id>"` 恢复会话。

[image](https://www.google.com/s2/favicons?domain=https://code.claude.com\&sz=32)

Claude Code Docs+1



实际执行时，必须让 Claude 处于 LayerFS Workspace 可见的文件系统环境中。建议在对应 Sandbox 中启动 tmux，而不是让宿主机 tmux 假设能直接访问容器内挂载点。

事件采集优先使用 Hooks，而不是解析终端文字。

Claude Hooks 当前能够提供 `session_id`、`transcript_path`、工具事件及 Session 生命周期信号。

[image](https://www.google.com/s2/favicons?domain=https://code.claude.com\&sz=32)

Claude Code Docs+1



| Claude 原生事件        | 统一事件                     |
| ------------------ | ------------------------ |
| SessionStart       | `session.native_started` |
| UserPromptSubmit   | `message.received`       |
| PreToolUse         | `tool.started`           |
| PostToolUse        | `tool.completed`         |
| PostToolUseFailure | `tool.failed`            |
| Stop               | `turn.stopped`           |
| SessionEnd         | `session.native_ended`   |

注意，`Stop` 表示模型本轮停止，不等于业务任务完成。

Codex 同样可以接入。第一版可使用 `codex exec --json` 获取结构化事件；需要长连接和细粒度交互时，优先使用 Codex app-server。

[image](https://www.google.com/s2/favicons?domain=https://developers.openai.com\&sz=32)

OpenAI Developers+1



### 4.3 LayerFS 的接入

使用官方 JavaScript SDK：

```
import { LayerFS } from "@layerfs/agent";

const fs = await LayerFS.open("my-project");

await fs.branch.fork("agent_001");

const { sandbox } = await fs.sandbox.create();

const ws = await fs.workspace.mount(
  sandbox,
  "agent_001"
);

// Agent 在 ws.path 对应的工作区执行。
// 每个重要步骤完成后显式提交。
const result = await fs.workspace.commit(
  ws,
  "agent checkpoint"
);
```

这是基于 LayerFS 1.1.0 的接口示意，需处理已有分支、错误和清理流程。LayerFS 支持 Workspace 复用及增量 Commit，但不会自动保存所有文件修改。

[image](https://www.google.com/s2/favicons?domain=https://github.com\&sz=32)

GitHub+1



这里的关键是 Commit 的触发归 Agent Bridge 管理：

- 工具调用结束，若文件发生变化，则提交 Checkpoint。
- 一轮任务结束，提交最终 Checkpoint。
- 长任务运行中定期检查脏数据，必要时提交。

LayerFS 保存文件版本后，Bridge 再把 `commit_id` 写入 Event Log。

## 五、Event Log：统一事件协议

建议 PostgreSQL 作为 V1 的可靠事件数据库。

Event Log 需要支持不同来源的事件统一关联。

Claude / Codex

LayerFS

Observer

Event Normalizer

统一 Schema · 去重 · 关联 ID

PostgreSQL · Append-only Event Log

推荐事件结构：

```
{
  "event_id": "evt_1024",
  "agent_id": "agent_001",
  "session_id": "session_001",
  "execution_id": "exec_045",
  "seq": 1024,
  "source": "layerfs",
  "source_event_id": "native_evt_99",
  "type": "artifact.committed",
  "ref": {
    "project": "my-project",
    "branch": "agent_001",
    "commit_id": "layerfs_commit_id"
  },
  "payload_ref": null,
  "timestamp": "2026-11-02T08:00:00Z"
}
```

所有事件由统一写入服务分配 Session 内的 `seq`。外部来源的 ID 用于去重，不能直接依赖多个系统的时间戳确定全局顺序。

### Log 和 LayerFS 如何关联？

以一次修改文件为例：

```
Execution 045
    │
    ├── Event: tool.started
    │
    ├── Claude 修改文件
    │       ↓
    │   LayerFS Workspace
    │       ↓
    │   Commit C128
    │
    ├── Event: artifact.committed
    │          commit_id = C128
    │
    └── Event: tool.completed
```

LayerFS Commit 管文件内容，Event Log 管这个 Commit 为什么发生。

两边不是原子事务。处理方式是先成功持久化文件版本，再写 Event。如果写 Event 失败，Bridge 必须重试并通过 `commit_id` 对账。

如果 LayerFS 的历史尚未可靠备份，Event 不能宣称文件已经永久持久化。

## 六、Observer：如何每秒检查一次

Observer 独立于计算运行，读取 Event Log 和必要的文件信息。

核心流程：

```
Heartbeat（每秒）
    ↓
读取 Session 最新 seq
    ↓
获取 Snapshot + Event 增量
    ↓
Jev 判断状态
    ↓
生成 Observation
    ↓
写入 Event Log
```

Jev 的判断结果可以是：

```
{
  "session_id": "session_001",
  "state": "BLOCKED",
  "reason": "WAITING_FOR_USER_PERMISSION",
  "based_on_seq": 1024,
  "confidence": 0.96
}
```

这些字段属于我们自定义的 Observer 输出协议，具体模型需要通过 Adapter 转换。

状态需要区分两类：

| 类型   | 示例              | 来源              |
| ---- | --------------- | --------------- |
| 执行事实 | 进程退出、工具完成、心跳失败  | Runtime / Event |
| 语义状态 | 正在工作、等待、阻塞、任务完成 | Jev             |

Jev 不负责决定是否唤醒 Agent。

唤醒来自消费层提交的命令、计算层已有的执行规则或持久化等待条件。Observer 只产生状态判断。

每个活跃 Session 保持 1Hz 检查目标。即使没有新事件，也可以重新评估超时等时间条件。但要确保 Observation 不会递归触发自己。

模型超时只标记 `UNKNOWN` 或观测过期，不能认为 Agent 已死亡。

## 七、SQL：如何自动呈现全部信息

这里建议将 SQL 层分成两部分，但它们仍属于同一个模块。

SQL Projector 负责从 Storage 自动生成结构化数据。

SQL Gateway 负责对消费者提供统一查询、订阅和受控命令入口。

### 7.1 自动提取流程

```
PostgreSQL Event Log ─── CDC ─────┐
                                 │
LayerFS Backend API ── Indexer ──┤
                                 ↓
                           SQL Projector
                                 ↓
                       Views / Materialization
                                 ↓
                           SQL Gateway
                                 ↓
                           Human / Agent
```

自动提取分两部分：

| 数据                            | 提取方法                     |
| ----------------------------- | ------------------------ |
| Session、Execution、Observation | Event Log 直接投影           |
| 文件、版本、Commit、Diff             | LayerFS Backend API 增量索引 |

通过 LayerFS Backend 读取 Commit 历史和文件版本，不直接依赖其内部 SQLite 私有表结构。

SQL Projector 保存处理游标，例如最近处理的 `seq` 和 LayerFS `commit_id`。断线后重新扫描并去重。

### 7.2 推荐的数据视图

| View               | 对上层暴露什么     |
| ------------------ | ----------- |
| `agent_state`      | Agent 当前状态  |
| `agent_timeline`   | 完整事件时间线     |
| `agent_tasks`      | 任务与进度       |
| `agent_artifacts`  | 文件及版本       |
| `agent_executions` | 执行历史和资源     |
| `agent_relations`  | 父子 Agent 关系 |

上层 Agent 读取：

```
SELECT *
FROM agent_state
WHERE agent_id = 'agent_001';
```

订阅状态变化：

```
SUBSCRIBE (
  SELECT *
  FROM agent_state
  WHERE agent_id = 'agent_001'
);
```

Materialize 目前已经支持 PostgreSQL CDC 和持续更新的 `SUBSCRIBE`。

[image](https://www.google.com/s2/favicons?domain=https://materialize.com\&sz=32)

Materialize Documentation+1



命令写入不能简单交给 Materialize。应由 SQL Gateway 接收受控的 `INSERT INTO agent_commands`，验证权限后写入 PostgreSQL。Compute 独立读取新命令，执行并写回事件。

这样消费者仍然只对接 SQL 层。

## 八、完整运行时序

以“上层 Agent 要求 Claude 修改一个项目”为例：

1. 提交任务

   上层 Agent 通过 SQL Gateway 写入命令，Event Log 记录任务请求。
2. 启动计算

   Compute Worker 读取命令，Agent Bridge 创建 LayerFS Workspace，并在对应环境启动 tmux + Claude。
3. 持续保存

   Claude Hooks 生成执行事件。文件写入 LayerFS，Bridge 显式 Commit，并将 Commit ID 写回 Event Log。
4. 持续观测

   Observer 每秒检查 Session，Jev 判断状态，结果写回 Event Log。
5. 自动呈现

   SQL Projector 获取事件和 Commit 变化，更新状态、进度与文件视图。
6. 继续执行

   上层 Agent 订阅到变化，自行决定下一步，并通过 SQL Gateway 下发新命令。

## 九、最需要解决的三个工程问题

### 1. 计算进程退出后如何恢复？

不能依靠 tmux 保存生命。

恢复过程是：读取 Agent Session → 找到最新可靠文件 Checkpoint → 重新挂载 LayerFS → 恢复原生 Claude/Codex 会话，或启动新会话并加载必要上下文 → 创建新 Execution。

原生 CLI 会话的恢复能力需要逐一验证。即使不能恢复原进程的内存状态，也必须保留完整的逻辑 Session 历史。

### 2. LayerFS 的存储耐久性

LayerFS 当前文档明确说明，运行中的进程、内存、未提交的 Workspace 变化，以及未另外保存的命令输出，都不属于 Commit。其当前 Store 配置也不能保证断电耐久性。

[image](https://www.google.com/s2/favicons?domain=https://layerfs.ai\&sz=32)

LayerFS docs+1



因此 V1 必须将 Session Event Log 放到独立可靠存储。关键文件还需要可靠备份或导出，不能只依赖 LayerFS 本地 Store。

### 3. 多 Agent 的并发与一致性

同一个 LayerFS Branch 上多 Agent 提交修改时，需要使用现成的冲突检测和 Merge 机制。

同一个 Session 的命令消费则需要租约、幂等键和执行序号。不能让两个 Worker 同时领取同一条命令，也不能在恢复后重复执行已确认的外部操作。

## 十、最小实现范围

| 组件        | 现成技术                   | 需要开发                  |
| --------- | ---------------------- | --------------------- |
| Compute   | tmux、Claude、Codex      | Agent Bridge          |
| 文件存储      | LayerFS                | Commit 关联与可靠备份        |
| Event Log | PostgreSQL             | 统一 Schema、事件写入        |
| Observer  | 定时器、Jev Adapter        | 1Hz 调度、状态映射           |
| SQL       | Materialize、PostgreSQL | Projector、SQL Gateway |
| Consumer  | 现有 Agent / SQL Client  | 少量 SDK 封装             |

V1 不要直接做多机分布式。 先在一台 Linux 主机上跑 LayerFS + Agent Bridge + tmux，把 Event Log 放在可靠 PostgreSQL 中。SQL 和 Observer 可以独立部署。

建议按以下顺序实施：

| 阶段              | 验收目标                                |
| --------------- | ----------------------------------- |
| M1：Agent Bridge | Claude 在 LayerFS 中执行，完整记录事件和 Commit |
| M2：Observer     | 单 Agent 持续 1Hz 判断，并恢复观测游标           |
| M3：SQL          | 查询和订阅状态、执行历史、文件产物                   |
| M4：恢复           | 强制退出 Claude/tmux，恢复同一个逻辑 Session    |
| M5：多 Agent      | 两个 Agent 并行分支执行、合并与冲突处理             |

## 最终设计判断

这套方案里，最有价值的接口是 Event Log 与 LayerFS Commit 之间的关联关系。

它让系统同时回答四个问题：

- 做了什么：Event Log。
- 产生了什么：LayerFS。
- 现在是什么状态：Jev Observation。
- 怎样对外呈现：SQL View。

而 Agent 可以是任何已有的 CLI 或 SDK，不需要重新开发它的智能和工具能力。

整个架构最终可以收敛为：

Compute 产生执行与文件变化 → Storage 持久化事实 → Observer 解释状态 → SQL 将事实和状态统一呈现 → 上层 Agent 持续消费。

工程上真正需要开发的是三个小核心：`Agent Bridge`、`Event Schema / Storage Index`、`SQL Projector / Gateway`。Jev Adapter 是可插拔组件。

这既保留了你要求的五个模块独立性，也让系统可以从单机原型逐步演进到长期运行的多 Agent 平台。
