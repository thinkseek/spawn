# Spawn 架构总览

[English README](../README.md) · [中文 README](../README.zh-CN.md) · [技术方案与接口契约](./design.md) · [视图层设计](./view-architecture.md) · [舰队架构](./fleet-architecture.md) · [存储](./storage.md) · [RSI](./rsi.md) · [升级](./upgrading.md)

结论：Spawn 用 12 个 Rust crate（`spawn-core`、`spawn-postgres`、`spawn-host`、`spawn-cas`、`spawn-jev`、`spawn-kdf`、`spawn-codegraph`、`spawn-node-proto`、`spawn-node-gateway`、`ghostty-vt`、`spawn-hc`、`spawn-cli`）与同仓 Go `spawn-node` 控制面前置单元连接现成组件，不实现 Agent 推理框架。Claude Code 提供计算，git（或 opt-in 的 D6 CAS Merkle/Ledger 存储）提供工作区，PostgreSQL 保存事件、对外提供 SQL 视图并以 `LISTEN/NOTIFY` 发出变更提示，Jev 判定语义状态，`spawn` 二进制提供控制核心与控制面（本地 UDS `spawn serve --ipc` 或直接 HTTP 服务，见 [spawn-ipc-v1.md](./spawn-ipc-v1.md)），独立 systemd Go 单元（`spawn-node-edge.service`、`spawn-node-gw.service`、`spawn-node-out.service`）分别承载 Thin Web + 外置 PWA 资源（`web/public/{manifest.webmanifest,sw.js,icon.svg}`）与浏览器端 WebCrypto `spawn-e2e-v2`、签名节点网关及私有出站 Jev RPC 代理，独立配套二进制 `spawn-hc` 承载内嵌 `herdr-core` 守护进程（`spawn` 自身不链接 `herdr-core` 与 `ghostty-vt`）。Spawn 自己的源码全部同步阻塞，不写 async（见第 10 节：`postgres` 驱动内部链接了 `tokio`；`herdr-core` 的异步运行时被严格隔离在独立的 `spawn-hc` 二进制中）。

## 1. 能力与边界

| 能力 | 实现方 | Spawn 做的事 |
| --- | --- | --- |
| 计算 | Claude Code | 启动进程、投递提示、读取 transcript、屏幕与退出文件 |
| 工作区 | git | 裸仓库加每分支一份工作树，`git archive` 导出、`commit-tree` 收口 |
| 事件存储 | PostgreSQL 16 | 建表、写入函数、视图、命令队列 |
| 语义状态 | 规则，加判定模型 `/v1/systemone`（未配置时只用规则） | 规则先判；规则定不了的情形发一次 HTTP 请求；写一条 observation |
| 对外接口 | SQL | 视图查询状态，`spawn_submit` 下发命令 |
| 终端 | Herdr（默认嵌入式 `herdr-core` `spawn-hc`，或 `SPAWN_HERDR_MODE=legacy` 选择外置 `herdr`；已移除并显式拒绝 `tmux`） | 建会话、输入文本、抓取屏尾 |
| 变更提示 | PostgreSQL `LISTEN/NOTIFY` | 每次追加事件 `pg_notify('spawn_events', <id>)`；`spawn loop` 和 `GET /api/stream` 收到即读，提示丢了也照读记录 |

Spawn 不实现：Agent 框架、提示工程、文件系统、状态机引擎。

`spawn serve` 提供 Thin Web + PWA 控制面（生产部署拆分运行：`spawn-core.service` 经 `%t/spawn/core.sock` 提供本地 UDS IPC，`spawn-node-edge.service` 对外承载 HTTP/SSE 与外置 PWA 资源；直接启动 HTTP 模式时默认绑 127.0.0.1，非回环地址必须设 `SPAWN_SERVE_TOKEN`；`SPAWN_SERVE_OPEN_NETWORKS` 列出的内网网段内、以该网段 IP 地址访问的客户端免 token）：`docs/api.md` / `ROUTES` 表内含 33 个 GET 路由模式和 9 个 POST 路由，另加 2 个契约锁定的表外只读 `GET|HEAD` 端点（`/api/features/truth` 与 `/api/episodes`，均在 `reads` 池以 `spawn_reader` 执行、非 `GET|HEAD` 返回 `405 Allow: GET`，见 [spawn-ipc-v1.md](./spawn-ipc-v1.md)）。GET 包括看板、事件查询、`GET /api/stream` 的 SSE 实时事件流、`#/grading` 视图（分级矩阵与 `/api/episodes` 控制流回放面板），以及把任务、落地、验证和部署分成 planned / implemented / verified / live 四态并由 `/api/features/truth` 提供真实代码依赖/纯度校验的能力图（`/api/features`、`#/features`，见 [feature-map.md](./feature-map.md)）。浏览器侧 `web/src/e2e.ts` 实现纯 WebCrypto `spawn-e2e-v2` 端到端密封信道（仅依赖标准 `crypto.subtle`）。POST 中 7 个是人的写入，都以 `spawn_human` 登录调用对应的 `spawn_human_*` 函数：对会话的输入、按键、给 supervisor 的消息、对审批提示的回答（`spawn_human_input`、`spawn_human_key`、`spawn_human_master_message`）经 `spawn_submit` 提交 principal 为 `human` 的命令；停滞 worker 的重启与改派（`spawn_human_task`）调用 `spawn_task_restart_as` / `spawn_task_reassign_as`，写 `task.restarted` 等控制记录，`by` 为 `human`；停摆任务的恢复（`spawn_human_resume`）写一条 `mission.resumed` 记录。另 2 个是能力图的重新扫描与文件关联。完整路由表、方法与 CLI 对应见 [api.md](./api.md)（`serve.rs` 与 `spawn_ipc_v1.rs` 契约测试保持一致）。发给 agent 会话的命令仍只经 `spawn_submit` 入队、由 Bridge 执行。

## 2. crate 结构

| crate | 职责 | 依赖 |
| --- | --- | --- |
| `spawn-core` | 类型、端口、强类型 `ControlRecord`（67 变体）/ `GradingRecord` ADT（`control.rs`）、任务 episode 回放、Bridge、Observer、能力图与依赖纯度引擎（`feature_deps`）；内存实现与一致性套件在 `testing` feature 后。零 IO；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | serde、serde_json、sha2、thiserror |
| `spawn-postgres` | EventLog + CommandQueue + RefLedger 的 PG 实现、`sql/schema.sql`、只读与 episode 校验方法、`LISTEN spawn_events` 连接（`wake.rs`）；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | spawn-core、spawn-kdf、postgres、postgres-protocol、fallible-iterator、serde、serde_json、whoami |
| `spawn-host` | 终端（`TerminalHost`：`HerdrCoreHost` 默认通过进程外 `spawn-hc` 运行，`HerdrPool` 按分片路由到 `<session>-s0..N`，`HerdrHost` 由 `SPAWN_HERDR_MODE=legacy` 选择，共享 OS 辅助位于 `terminal_support`，`tmux` 后端已移除并显式拒绝；`herdr_witness` 宿主机第二证人；`BwrapView`/`ContainerView` 沙箱隔离）、工作区存储（`LocalWorkspaceStore`：默认 `git` `GitWorkspace` 与按需启用/无 `git` 子进程的纯 Rust `d6` `D6Workspace` 两种后端，同一 `WorkspaceStore` 契约，工作区复制隔离位于 `reflink.rs` 的 `clone_or_copy`）、transcript 归档、inotify；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | spawn-core、spawn-cas、libc、rustix、serde_json |
| `spawn-cas` | 统一内容寻址存储：带算法前缀的摘要（`sha256:<64 hex>`）、`objects/<algo>/<2 hex>/<hex>`（0444）、`pins/<owner>/<name>`、D6 Merkle 树/日志存储、按命名空间的 LRU GC（`access/<ns>/`）、`tmp/` 暂存；crash-only，不 fsync；与 Go `spawn-node/cas` 同一磁盘布局；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | sha2、thiserror、libc |
| `spawn-jev` | 判定 HTTP 客户端（支持直连 HTTP/1.1 与 `SPAWN_OUT_SOCK` 本地 `out.sock` UDS 通道及 `validate_out_sock` 安全校验），端口在 core，实现在此；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | spawn-core、serde、serde_json |
| `spawn-kdf` | 受限登录口令的 PBKDF2-HMAC-SHA256 派生（单独一个 crate，开发构建也以 `opt-level = 3` 编译）；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | sha2、hmac |
| `spawn-codegraph` | 能力图的代码依赖事实：从仓库文件（Rust / JS-TS / Python）抽取代码图；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | spawn-core、syn、proc-macro2 |
| `spawn-node-proto` | spawn-node v1 线协议：签名请求信封与响应、大小上限、严格 JSON、heartbeat/spec/grant/result/enroll 报文、`tree_sha256_v1`（即 `scenarios_sha256`），以及与 Go 守护进程共用的 golden fixtures（`testdata/node-protocol-v1/`）；声明 `#![forbid(unsafe_code)]` 与 `#![warn(missing_docs)]` | ring、sha2、base64、serde、serde_json、thiserror |
| `spawn-node-gateway` | 二进制 `spawn-node-gateway`：spawn-node v1 网关，节点与账本之间唯一的门。同步的 std HTTP/1.1 服务（16 个 worker，各持一条 PG 连接），以受限登录 `spawn_node_gateway` 调用四个 `spawn_node_gw_*` 函数（先验签再提交），签名每个应答与 grant，流式落盘 result，按 Range 提供 artifact；协议见 `docs/node-protocol-v1.md` | spawn-node-proto、spawn-postgres、ring、sha2、base64、serde、serde_json、zeroize |
| `ghostty-vt` | 供 `[patch."https://github.com/herdrdev/herdr-core"]` 使用的纯 Rust VT100/xterm/OSC-7501 仿真替代层（实现 `Terminal`、`StatusReport`、`Rgb`，零 Zig / 零 C FFI / 零 `build.rs`） | serde、serde_json |
| `spawn-hc` | 独立配套二进制 `spawn-hc`：封装 `herdr-core` CLI/守护进程与 OpenRPC Unix socket 入口，将 `herdr-core` 及其 tokio 运行时与主 `spawn` 二进制进程外隔离 | herdr-core（及patched `ghostty-vt`） |
| `spawn-cli` | 二进制 `spawn`，装配上述实现（不含 `herdr-core`/`ghostty-vt`，通过子进程与 Unix socket 驱动 `spawn-hc`）；`spawn serve` 的 HTTP 服务、SSH 节点管理与 `spawn-node` v1 运维命令 | 上述 core/host/pg 等实现层，以及 tiny_http、rustix、ring 等 |

```mermaid
flowchart LR
  core[spawn-core<br/>types · ports · bridge · observer]
  kdf[spawn-kdf]
  pg[spawn-postgres]
  jev[spawn-jev]
  host[spawn-host]
  cas[spawn-cas]
  graph[spawn-codegraph]
  proto[spawn-node-proto]
  gw[spawn-node-gateway]
  vt[ghostty-vt]
  hc[spawn-hc]
  cli[spawn-cli]
  pg --> core
  pg --> kdf
  jev --> core
  host --> core
  host --> cas
  graph --> core
  cli --> pg
  cli --> jev
  cli --> host
  cli --> graph
  cli --> proto
  hc --> vt
  gw --> proto
  gw --> pg
```

依赖单向：只有 `spawn-cli` 同时依赖 `spawn` 各同步实现层；独立配套二进制 `spawn-hc` 仅封装 `herdr-core`（经 Cargo `[patch]` 使用纯 Rust `ghostty-vt`），与 `spawn` 二进制完全隔离，因此 `spawn` 不引入 `herdr-core`、`ghostty-vt` 或任何新增异步依赖；实现层之间的依赖只有两条：`spawn-postgres` 用 `spawn-kdf` 派生登录口令，`spawn-host` 的 `D6Workspace` 与 cutover/border 适配器基于 `spawn-cas` 存储内容寻址对象与 Merkle 树。`spawn-node-gateway` 是另一个二进制，只依赖 `spawn-node-proto` 与 `spawn-postgres`。

## 3. 一次任务的链路

```text
spawn_submit(principal, key, agent, session, action)
  → command.accepted          PG 事务，幂等
  → spawn_claim(worker, ttl)  单会话串行，租约 fencing
  → git branch / archive 导出工作树
  → spawn-hc / herdr 创建终端  运行 spawn run -- claude --ax-screen-reader --session-id <uuid>
  → 屏尾判定 Ready             终端 read / agent.get 尾行解析
  → agent.prompt / send-keys   投递提示
  → Claude transcript         Bridge 每轮读 <projects>/<工作区解析后的路径>/<uuid>.jsonl，claude_line 归一为统一事件
  → 屏幕                      Bridge 每轮抓屏，两次读数一致才写 screen.observed
  → 退出文件                  spawn run 把退出码写入 .spawn/exit-<execution>，Bridge 读到后写 execution.ended
  → spawn_append              写 spawn_events，双序号
  → Observer tick             规则或模型判定语义状态
  → observation.recorded
  → agent_state / agent_timeline 视图
```

两条链路互不依赖：数据链路是引擎到事件到 SQL，状态链路是事件到判定再回到事件。

worker 的终端不持有数据库登录，引擎也不向 Spawn 汇报：会话 id 由 Bridge 用 `--session-id` 指定（由执行 id 派生的 UUID），因此引擎启动前 transcript 路径就已确定。

工作区只有一个名字，即引擎进程看到的那个。Herdr 在工作区目录启动终端进程，内核沿途解析所有符号链接，引擎的工作目录（`getcwd`、`pwd -P`）是解析后的路径，Claude Code 把 transcript 写在这个路径（除 ASCII 字母和数字外的字符都换成 `-`）对应的项目目录下。所以工作区根在配置处解析一次（`spawn_host::canonical_root`：`spawn-cli` 的 `workspace_root`，以及铸造工作区 id 的 `GitWorkspace::new`），mount、终端启动工作目录、`WorkspaceId`、runner 的退出文件和 transcript 路径都出自这一个 id，不会分叉。根经由符号链接到达的主机（`/home -> /data00/home`；macOS 的 `/tmp`、`/var` 指向 `/private/...`）以前会让 Bridge 盯着一个永远收不到数据的目录（issue #15）。旧版本按配置原样记录的 id 仍指向同一目录：`WorkspaceStore::canonical` 把它映射到解析后的根下，Bridge 读日志里每个执行的工作区都经过它，`GitWorkspace` 的路径检查也接受它（docs/storage.md「Workspace identity」）。

一个执行何时算结束：runner 写了退出文件（`exited`）；或者没有 Bridge 自己启动的 runner 在跑它（`lost`）：终端不在了，或者终端还在但不托管那个 runner（或重启后仅余未关联本机记录与 leader PID 的同名终端）。`HerdrCoreHost` / `HerdrHost` 通过守护进程终端列表、私有记录文件中的服务器与会话 leader 身份以及本次启动的环境文件名（`spawn-launch-<终端>-`）核验 runner 归属。`lost` 的 `execution.ended` 带 `evidence`，记下主机看到了什么。start、resume、input 在动手前先处理这种执行（`Bridge::settle`）：照常走结束流程（最终 checkpoint、杀终端、卸载、写结束事件）后，start/resume 继续，input 失败一次并提示 resume；stop 写下日志在等的结束（`stopped`，同样带 evidence）。Bridge 对“谁在运行”的视图只随折叠日志改变；视图为空时向日志查询该会话未结束的执行（`EventLog::open_execution`，与 `spawn_append_trusted` 的守卫同一规则），所以视图与日志即便分叉也会收敛。不会静默收编不受跟踪的引擎：它会被结束，终端被杀。

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
| `spawn_claim(worker, ttl_ms)` | 领取命令，单会话串行（记了 `waiting` 的输入不挡后面的命令），过期租约 attempt+1 |
| `spawn_release(command, attempt, ms)` | 不结束命令，租约 `ms` 毫秒后到期（等轮次的输入用） |
| `spawn_finish(...)` | 结束命令，返回 `done`、`already`、`stale`、`missing` |
| `agent_state` | 每会话最新 observation |
| `agent_timeline` | 会话事件时间线 |
| `agent_tasks`、`agent_executions`、`agent_artifacts`、`agent_relations` | 任务、执行、产物、父子关系 |
| `agent_commands` | 命令视图；`insert` 经 `spawn_gateway_insert` 校验 `spawn_grants` 后入队 |

视图不用物化视图和 CDC。订阅方式是读取 `spawn_events` 增量，`LISTEN/NOTIFY` 只做提示：`spawn_append` 每写一条事件就 `pg_notify('spawn_events', <事件 id>)`。`spawn loop` 持一条 `LISTEN spawn_events` 连接，收到通知即结束等待、读水位（`max(spawn_events.id)`）；`spawn serve` 的 `GET /api/stream` 由一个监听线程以只读登录 `LISTEN`，读新事件后扇出给每个 SSE 流。通知丢了、晚了或没发都不算“没有变化”：两者都按时读记录本身（docs/design.md §14）。

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
| `submit` / `say` | `submit` 写一条命令，幂等键重复则返回原命令（省略 `--session`/`--key` 时自动解析单活会话与生成幂等键）；`say <agent> <message>` 为向 agent 发送 `human` 人工输入的简写命令 |
| `tick` | 跑一轮 Bridge 与 Observer |
| `run` | 在终端内启动引擎，退出时把退出码写入 `SPAWN_EXIT_FILE`（Bridge 读到后写 `execution.ended{Exited}`），转发退出码；不连数据库 |
| `loop` | 常驻：每 500 ms 跑一轮 Bridge、Observer、派发与存活规则（`SPAWN_LOOP_INTERVAL_MS`） |
| `integrate` | 常驻：对已验收任务跑黑盒门禁，宿主机提交并以 CAS 前移来源基线 |
| `serve` | 常驻：本地 IPC / Thin Web + PWA 控制面，33 个 GET 路由模式和 9 个 POST 路由（[api.md](./api.md)），另加 `/api/features/truth`、`/api/episodes` 两条表外只读 `GET\|HEAD` 端点（[spawn-ipc-v1.md](./spawn-ipc-v1.md)） |
| `scorecard` | 只读记录，一次量出八个维度（吞吐、前导时间、一次通过率、返工、自主、可靠、成本、门禁耗时），带 n、方向、上一窗口与趋势（[design.md](design.md) §15） |
| `mission` / `task` / `resident` | 任务域控制流：添加、配置、关闭、恢复任务，回放并校验 episode 投影一致性（`spawn mission episodes [--mission <id>] [--strict] [--verify-parity] [--json]`），分派（含 `spawn_objective_assign` 难度与 preset 路由）与验收子任务，提议、批准、退役常驻 worker |
| `features` | 能力图与代码图：`scan` / `associate` / `graph` / `check [--deps report\|enforce] [--purity report\|enforce]` |
| `status` / `fleet` / `tail` | `status [--json]` 单命令总览活跃会话、未结任务与告警；`fleet`（每个会话及其判定）、`tail`（单个会话的判定与事件尾随） |
| `doctor` | 诊断配置、存储、沙箱、工具链与 `model_endpoints` 健康探针（`--config` / `--root` / `--json`） |
| `version` | 打印 `spawn <版本>` |

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PG 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 不属于任何任务的 agent（supervisor、临时 worker）用的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 每个分支一份工作树的根目录；启动时解析符号链接一次，工作区 id 都在解析后的根下 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude 可执行文件 |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`，否则 `~/.claude/projects` | Claude 写 transcript 的目录 |
| `SPAWN_MAX_FILE_MB` | `100` | Git 与 D6 checkpoint 收录的单个文件上限（MiB）；更大的文件不进 checkpoint，并告知 agent 一次 |
| `SPAWN_WORKSPACE_BACKEND` | `git` | 根下所有工作区的引擎：`git`（默认，每个分支一份 Git 工作副本）、`d6`（按需启用的纯 Rust `D6Workspace`，零 `git` 子进程，见 [d6-cas-v1.md](./d6-cas-v1.md)），以及 `auto`（遵循根标记或既有 `.d6/`/`.sources`/工作区，干净根目录选 `git`）。根兼容性矩阵：纯 `d6` 严禁与 `git` 混用；不兼容或未知取值（含已退场的 `cow`/`overlay`/`d6-shadow`）以及未标记的旧 `.cow/`/`.overlay/`/`.d6-shadow/` 目录时启动即 fail-closed 拒绝 |
| `SPAWN_D6_CAS_DIR` | 未设 | `d6` 的 CAS Merkle 与 `refs.journal` 目录覆写（绝对路径） |
| `SPAWN_D6_TMPFS_ROOT` | 未设 | `d6` 的有界 L1 `tmpfs` upperdir 根目录（绝对路径） |
| `SPAWN_D6_TMPFS_BUDGET_MB` | `2048` | L1 `tmpfs` 硬预算（MiB；设 `SPAWN_D6_TMPFS_ROOT` 时生效） |
| `SPAWN_TERMINAL_BACKEND` | `herdr` | 唯一终端后端：`herdr`（默认，经 `SPAWN_HERDR_MODE=core` 路由到内置 `spawn-hc`）；`tmux` 已移除并在环境变量与 `spawn.toml` 中显式拒绝 |
| `SPAWN_HERDR_MODE` | `core` | herdr 后端模式：`core`（默认，使用同目录 `spawn-hc`）或 `legacy`（外置 legacy `herdr`） |
| `SPAWN_HERDR_CORE_PROGRAM` | 同目录 `spawn-hc` | 内嵌 `herdr-core` 模式的可执行文件覆写路径 |
| `SPAWN_HERDR_SHARDS` | `1` | Herdr 服务分片数（`1..=64`；大于 1 时按终端名确定性路由到 `<session>-s0..N`） |
| `SPAWN_HERDR_PROGRAM` | `herdr` | legacy `herdr` 可执行文件（`SPAWN_HERDR_MODE=legacy` 时） |
| `SPAWN_HERDR_SESSION` | `spawn` | `spawn-hc` / legacy `herdr` 私有会话名 |
| `SPAWN_JUDGE_URL` | 未设 | 判定服务根地址（直连 HTTP/1.1），与 `SPAWN_OUT_SOCK` 均未设时只用内置规则 |
| `SPAWN_OUT_SOCK` | 未设 | 本地 `out.sock` UDS 路径（`spawn-node-out.service` `%t/spawn/out.sock`，走 `POST /out/v1/jev/call`；连接前校验属主、非符号链接及 `0600`） |
| `SPAWN_WORKER` | `spawn-worker` | 租约中的 worker 名 |
| `SPAWN_LOOP_INTERVAL_MS` | `500` | `spawn loop` 两轮之间的间隔 |
| `SPAWN_MODEL_PRICES` | 内置价目表 | 各模型每百万 token 的美元价格，`{"<模型>": {"input", "output", "cache_write", "cache_read"}}`，每项为不小于 0 的数：每次模型调用在记录时按它定价（Cost 页、`GET /api/cost`、任务的 `--daily-usd` 日预算）。未定价的模型只计 token、不计费用（记为 unpriced）。设置后整体替换内置表（docs/fleet-architecture.md §3.5）；不是这样的对象时 `spawn loop` 和 `spawn serve` 启动即退出 |
| `SPAWN_SCREEN_QUIET_MS` | `500` | 活跃执行的会话记录要静止多久才读一次屏（读的是会话记录说不了的：提示符回来了）。越短越早知道就绪，抓屏也越多 |
| `SPAWN_DELIVERY_RETRY_SECS` | `60` | 投递失败（什么也没打进去）后第一次计数重试前的等待（整秒，不是非负整数时在 loop 日志里警告并用默认值），之后每次加倍，至多 30 分钟；共重试 3 次（窗格未就绪的前 10 次拒绝不计） |
| `SPAWN_DELIVERY_NOT_READY_MS` | `5000` | 投递因窗格未就绪被拒（什么也没打进去）后再试前的等待（毫秒，至少 100；不是这样的整数时在 loop 日志里警告并用默认值）。只决定何时再试，不改次数：前 10 次这样的拒绝始终不计。免计的是次数而不是时长，所以窗格在拒绝开始计数前得到的宽限约为 10 倍这个等待（默认约 50 秒，019 的 500 毫秒约 5 秒，下限约 1 秒）；更慢的窗格随后付计数重试的退避 |
| `SPAWN_REISSUE_AFTER_SECS` | `300` | 首次投递的重试用尽、且没有一次可能打进去时，从最后一次尝试结束到重发（取消并重新分派一次，`spawn_task_reissue`）的等待（整秒，同上）。给一次部署修好主机的时间；部署后（`source.moved`）每个用尽的投递会立即再试一次 |
| `SPAWN_STALL_DELIVERY_SECS` | `600` | 停滞上限（整秒，下同；不是非负整数时在 loop 日志里警告并用默认值）：待投递的任务多久没送达就升级给 supervisor。远长于第一次重试（60 s）和工人结束一轮所需的时间 |
| `SPAWN_STALL_ACTIVITY_SECS` | `900` | 已投递的任务，会话多久没有新事实就升级。一次长构建或测试可以几分钟不写任何东西；工人回合已结束、而它启动的后台任务（会话记录里的 `job.started` 没有 `job.ended`）仍开着时算作活动，不升级 |
| `SPAWN_STALL_REVIEW_SECS` | `1800` | 报告等 supervisor 审阅多久就升级：审阅要读报告、看代码，supervisor 可能正忙 |
| `SPAWN_STALL_GATE_SECS` | `3600` | 带门禁的任务等集成器判定多久就升级：门禁自己的墙钟是 120 分钟（`SPAWN_GATE_WALL_CLOCK_MINUTES` 的默认值） |
| `SPAWN_STALL_LANDING_SECS` | `600` | 门禁通过后多久没落地就升级：落地只是一次合并加推送，集成器下一趟就做 |
| `SPAWN_STALL_STOP_SECS` | `300` | 关闭的临时任务，工人多久还在跑就升级：退役 Stop 在下一趟就发，失败的 Stop 一分钟后重发 |
| `SPAWN_STALL_HALTED_SECS` | `0` | 停摆的任务（mission halted）多久后升级：停摆从一开始就在等人 |
| `SPAWN_STALL_JOB_SECS` | `7200` | 工人的一个后台任务（Bash 后台命令、Monitor）开着多久后升级一次（“a background job has run for …”，带种类与 id）。在此之前，等着自己后台任务的工人算在工作：不问 Jev、不升级、不催、不算停滞。Monitor 一小时内自己到期；门禁一次约半小时 |
| `SPAWN_GATE` | `scripts/rsi/gate.sh` | `spawn integrate` 的门禁脚本，输出一行 JSON 判定；线上指向控制器（部署的提交树）`~/.local/share/spawn-rsi/controllers/spawn/scripts/rsi/gate.sh`，从不指向所有者的检出：所有者的改动部署后才生效 |
| `SPAWN_INTEGRATE_GATES` | `1` | `spawn integrate` 同时跑的门禁数（跨任务组；每个任务组至多一个；`--gates` 优先）。大于 1（至多 8）须设 `SPAWN_TEST_DATABASE_PER_GATE=1`：门禁目前共用一个测试数据库服务器。场景目录里有 `no-test-database` 文件的任务组（如 Supply PA，测试用 PGlite）拿不到测试库地址，其判定不占这个名额，可与 Spawn 的门禁并行 |
| `SPAWN_INTEGRATE_DRAIN_SECS` | `7200` | `spawn integrate` 排空（SIGUSR1，或同一 `--root` 的 `spawn integrate --drain`）的上限秒数：正在跑的门禁记录判定并落地，不再起新门禁，之后以 0 退出；到上限时停掉仍在跑的门禁，`integrator.drain_expired` 记下它们的任务。1 到 86400 的整数，其他值启动即拒绝 |
| `SPAWN_GATE_JOBS` | 未设 | 每个门禁同时跑的场景数（大于等于 1 的整数）。`spawn integrate` 总是把每个门禁的值传给 gate.sh：设了就用它；未设时，共用测试服务器的门禁为 1，其他门禁为（核数 − `SPAWN_GATE_WORKER_CORES`）÷ 能同时跑的门禁数（每个源一个，共用服务器的至多 `--gates`），至多 8。手跑 gate.sh 时未设则为 min(8, 核数/2) |
| `SPAWN_GATE_HOST_JOBS` | 未设 | 整台主机的场景预算（大于等于 1 的整数）：每个门禁的每一次场景运行都先在 `$HOME/.cache/spawn/gate-slots` 下用 flock 领一个槽位，所有门禁合计不超过这个数。优先看主机自己的陈述文件 `$HOME/.cache/spawn/gate-slots/host_jobs`（部署从集成器的门禁环境写入，所有者可手改；它优先于本变量，所以手跑的门禁带别的值也抬高不了上限）；没有文件时用本变量，再没有则等于 `SPAWN_GATE_JOBS`（所有者设了的话），否则等于手跑 gate.sh 的默认值 min(8, 核数/2)。槽位在 gate.sh 内领取，谁起的门禁（集成器的、手跑的、部署预热的）都算同一份预算，不需要任何配置；进程结束（被杀也算）时内核释放锁，所以不丢槽位。等待中的运行会先打印 "waiting for a host scenario slot"，随后在自己的块里记录等了多久、拿到第几个槽位。构建钩子、测试清单（tests: what cargo runs）和快照不占槽位（每个门禁各一次，门禁本来就不多） |
| `SPAWN_GATE_SLOT_WAIT_SECS` | `1800` | 一次场景运行为等主机的场景槽位最多等多久（整秒，从 1 起）。到点后门禁说“现在没有地方判定”（exit 5），把 `{"no_room":true,"check":"host scenario slot","waited_ms","budget"}` 写进 verdict 文件（也打印到 stdout，写入失败时集成器读最后一行），然后退出，这个场景不启动；任务按 `no_room` 的 backoff（5 分钟起，每次加倍，至多 40 分钟）再判，2 小时后升级一次。要短于集成的门禁墙钟（`SPAWN_GATE_WALL_CLOCK_MINUTES`，默认 120 分钟）；`spawn integrate` 不传它，测试用它把 30 分钟缩成秒 |
| `SPAWN_GATE_WORKER_CORES` | 核数的四分之一（至少 1） | 给工人和主机留的核数（0 到核数 − 1 的整数，其他值使 `spawn integrate` 不启动并报出变量名）。门禁只在它的场景数 + 正在跑的门禁的场景数 + 这个数不超过核数时才开始（没有门禁在跑时总能开始），否则下一趟再试，不记录、不算失败 |
| `SPAWN_TEST_DATABASE_LOG` | 未设 | 测试数据库服务器的日志（`log_line_prefix` `%m [%p] `）；`spawn integrate` 读它找门禁期间的崩溃：有崩溃时数据库错误也让 check-tests.sh 的失败算主机故障（`host_fault`）而非拒绝。未设时只有连接错误算 |
| `SPAWN_GATE_HOST_FAULT_BACKOFF_SECS` | `300` | 连续第一次主机故障后，任务等多久再判；每多一次连续主机故障加倍，至多一小时 |
| `SPAWN_SOURCE_HOLD_SECS` | `10800` | `spawn mission hold` 的上限（整秒，1 到 86400；其他值拒绝并报出变量名，不换成默认值）：暂停记录的 `expires_ms` 是它自己的时间加这么多，之后集成器记 `source.hold_expired` 结束它。3 小时长于最慢的所有者门禁（80 分钟）加一次部署，也长于门禁默认的墙钟（120 分钟） |

## 8. 验证

```bash
scripts/rsi/gate.sh "$PWD/.git" HEAD HEAD "$PWD/rsi/scenarios"   # 唯一门禁：构建、clippy、测试、e2e、黑盒、web
scripts/rsi/test-gate.sh                                          # 门禁自身的测试
```

检查项都是 `rsi/scenarios/check-*.sh` 场景（包括 `check-features.sh` 校验 `docs/feature-map/registry.json` 与 `spawn features check --deps` / `--purity` 代码边界），见 [fleet-architecture.md](./fleet-architecture.md) §10.2。此外，Keep 凭据治理脚本 `scripts/keep/credential-health.sh` 支持 `unix://<socket>|<path>` UDS 健康探针，并通过 `go_unit_pg_isolation` 检查强制要求 `spawn-node-{edge,gw,out}.service` 及其 drop-in 零 `spawn.env` 与 `*DATABASE_URL*`；`spawn-core` 测试 `crates/spawn-core/tests/dependency_guard.rs` 永久校验 8 个已审计核心/数据/宿主/协议 crate（`spawn-core`、`spawn-postgres`、`spawn-host`、`spawn-node-proto`、`spawn-cas`、`spawn-kdf`、`spawn-codegraph`、`spawn-jev`）的 `#![forbid(unsafe_code)]`、`#![warn(missing_docs)]` 与 Phase 2d `std::net` 棘轮。

门禁一次运行自包含、无状态，所以可以离开本机：`deploy/gate-runner.sh <runner> <source.git> <base> <candidate> <scenarios> <out>` 把门禁所需（基线与候选的 git bundle、控制器的脚本与场景、本次作业的运行脚本）打包，经 SSH 送到远端 runner，在那里用 runner 自己的私有 PostgreSQL 和临时目录跑同一个 `scripts/rsi/gate.sh`，再把判定和完整日志带回（`<out>/<job>/{verdict.json,gate.log,record.json}`）。runner 拿不到主库地址；重试只针对传输，从不针对判定；判定环境与本机不一致的 runner（能看到主机文件、内存低于下限）被拒绝。`--check` 只验证 runner 是否会和本机判得一样。

## 9. 多主机

主机之间遵循三条原则：轻通信多保活 · 事件同步 · 数据库隔离。

- 轻通信多保活：主机之间只传小消息（节点心跳每 15 s 一次的小探针、只读 inventory），不共享文件系统、不长连接搬数据；保活靠多处独立检查（`spawn loop` 的心跳线程、`node.lost` / `node.back`、告警），任何一处失败不阻塞主循环。
- 事件同步：主机状态只经事件日志同步（`node.joined`、`node.heartbeat`、`node.lost`、`node.back`、`node.drained` 等），读方看 `GET /api/nodes` 或 `spawn node list`，不另设状态通道。
- 数据库隔离：控制面和唯一的事件库在 primary；节点和远端门禁 runner 不持有主库登录（远端门禁用 runner 自己的私有 PostgreSQL），URL 从不出现在命令行上。

节点的加入、探测和低电量软排空见 [fleet-architecture.md](./fleet-architecture.md) §1.2。

## 10. 依赖约束

“零 async”是 Spawn 自己源码的纪律：Spawn 的代码里没有 `async fn`、`.await`，也不启动、不持有 async 运行时；需要等待的地方用线程、阻塞调用和超时。依赖内部不受此限：阻塞的 `postgres` 驱动是 `tokio-postgres` 的同步包装，内部链接并驱动 `tokio`，这不改变 Spawn 源码同步阻塞的事实。不使用 chrono、uuid、rand，不使用 HTTP 客户端库。时间统一为 `i64` 毫秒。`spawn-jev`（`#![forbid(unsafe_code)]`、`#![warn(missing_docs)]`）由标准库 socket 手写同步客户端，支持直连 HTTP/1.1（`Transfer-Encoding: chunked`）以及经 `SPAWN_OUT_SOCK` 本地 UDS 拨号 `spawn-node-out.service`（`/out/v1/jev/call`，拨号前做非符号链接 / 属主 UID / `0600` 校验）；上游 API Key 仅由 `spawn-node-out.service` 通过 systemd `LoadCredential=typesafe.api-key:/run/keep/typesafe.api-key` 注入，不进 Spawn 主环境变量。
