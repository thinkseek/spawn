# Spawn 架构总览

[English README](../README.md) · [中文 README](../README.zh-CN.md) · [技术方案与接口契约](./design.md) · [视图层设计](./view-architecture.md) · [舰队架构](./fleet-architecture.md) · [存储](./storage.md) · [RSI](./rsi.md) · [升级](./upgrading.md)

结论：Spawn 用 5 个 Rust crate 连接现成组件，不实现 Agent 推理框架。Claude Code 提供计算，git 提供工作区，PostgreSQL 保存事件并对外提供 SQL 视图，Jev 判定语义状态，`spawn` 二进制只做装配。全部代码同步阻塞，不使用 async。

## 1. 能力与边界

| 能力 | 实现方 | Spawn 做的事 |
| --- | --- | --- |
| 计算 | Claude Code | 启动进程、投递提示、读取 transcript、屏幕与退出文件 |
| 工作区 | git | 裸仓库加每分支一份工作树，`git archive` 导出、`commit-tree` 收口 |
| 事件存储 | PostgreSQL 16 | 建表、写入函数、视图、命令队列 |
| 语义状态 | 规则，加判定模型 `/v1/systemone`（未配置时只用规则） | 规则先判；规则定不了的情形发一次 HTTP 请求；写一条 observation |
| 对外接口 | SQL | 视图查询状态，`spawn_submit` 下发命令 |
| 终端 | tmux | 建会话、输入文本、抓取屏尾 |

Spawn 不实现：Agent 框架、提示工程、文件系统、状态机引擎。`spawn serve` 提供看板和事件查询（默认绑 127.0.0.1，非回环地址必须设 `SPAWN_SERVE_TOKEN`；`SPAWN_SERVE_OPEN_NETWORKS` 列出的内网网段内、以该网段 IP 地址访问的客户端免 token），不是控制面；它唯一的写入是人对某个会话的输入（`POST /api/session/<id>/input`，以 `spawn_human` 登录调用 `spawn_human_input`，即经 `spawn_submit` 提交 principal 为 `human` 的 input）；所有写入仍只经 Bridge 和 `spawn_submit`。

## 2. crate 结构

| crate | 职责 | 依赖 |
| --- | --- | --- |
| `spawn-core` | 类型、端口、事件归一、Bridge、Observer；内存实现与一致性套件在 `testing` feature 后。零 IO | serde、serde_json、thiserror |
| `spawn-postgres` | EventLog + CommandQueue 的 PG 实现、`schema.sql`、读方法 | spawn-core、postgres、sha2 |
| `spawn-jev` | 判定 HTTP 客户端，端口在 core，实现在此 | spawn-core、serde_json |
| `spawn-host` | tmux、git 工作区与块存储适配 | spawn-core、libc、blake3、zstd |
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
  → git branch / archive 导出工作树
  → tmux new-session          运行 spawn run -- claude --ax-screen-reader --session-id <uuid>
  → 屏尾判定 Ready            capture-pane 尾行解析
  → tmux send-keys            投递提示
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

工作区只有一个名字，即引擎进程看到的那个。tmux 用 `-c` 在工作区启动窗格，内核沿途解析所有符号链接，引擎的工作目录（`getcwd`、`pwd -P`）是解析后的路径，Claude Code 把 transcript 写在这个路径（除 ASCII 字母和数字外的字符都换成 `-`）对应的项目目录下。所以工作区根在配置处解析一次（`spawn_host::canonical_root`：`spawn-cli` 的 `workspace_root`，以及铸造工作区 id 的 `GitWorkspace::new`），mount、窗格的 `-c`、`WorkspaceId`、runner 的退出文件和 transcript 路径都出自这一个 id，不会分叉。根经由符号链接到达的主机（`/home -> /data00/home`；macOS 的 `/tmp`、`/var` 指向 `/private/...`）以前会让 Bridge 盯着一个永远收不到数据的目录（issue #15）。旧版本按配置原样记录的 id 仍指向同一目录：`WorkspaceStore::canonical` 把它映射到解析后的根下，Bridge 读日志里每个执行的工作区都经过它，`GitWorkspace` 的路径检查也接受它（docs/storage.md「Workspace identity」）。

一个执行何时算结束：runner 写了退出文件（`exited`）；或者没有 Bridge 自己启动的 runner 在跑它（`lost`）：终端不在了，或者终端还在但不托管那个 runner（窗格已死、`remain-on-exit`，或重启后终端管理器恢复出来、停在 shell 提示符的同名会话）。tmux 适配器用 `list-panes` 读 `pane_dead` 和 `pane_start_command`，后者必须带上本次启动的环境文件名（`spawn-launch-<终端>-`）。`lost` 的 `execution.ended` 带 `evidence`，记下主机看到了什么。start、resume、input 在动手前先处理这种执行（`Bridge::settle`）：照常走结束流程（最终 checkpoint、杀终端、卸载、写结束事件）后，start/resume 继续，input 失败一次并提示 resume；stop 写下日志在等的结束（`stopped`，同样带 evidence）。Bridge 对“谁在运行”的视图只随折叠日志改变；视图为空时向日志查询该会话未结束的执行（`EventLog::open_execution`，与 `spawn_append_trusted` 的守卫同一规则），所以视图与日志即便分叉也会收敛。不会静默收编不受跟踪的引擎：它会被结束，终端被杀。

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

视图不用物化视图和 CDC。订阅方式是读取 `spawn_events` 增量，没有 `LISTEN/NOTIFY`。

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
| `submit` | 写一条命令，幂等键重复则返回原命令 |
| `tick` | 跑一轮 Bridge 与 Observer |
| `run` | 在终端内启动引擎，退出时把退出码写入 `SPAWN_EXIT_FILE`（Bridge 读到后写 `execution.ended{Exited}`），转发退出码；不连数据库 |
| `loop` | 常驻：每 500 ms 跑一轮 Bridge、Observer、派发与存活规则（`SPAWN_LOOP_INTERVAL_MS`） |
| `integrate` | 常驻：对已验收任务跑黑盒门禁，宿主机提交并以 CAS 前移来源基线 |
| `serve` | 常驻：只读看板与 HTTP 查询 |
| `scorecard` | 只读记录，一次量出八个维度（吞吐、前导时间、一次通过率、返工、自主、可靠、成本、门禁耗时），带 n、方向、上一窗口与趋势（[design.md](design.md) §15） |
| `mission` / `task` / `resident` | 任务域控制流：添加、配置、关闭、恢复任务，分派与验收子任务，提议、批准、退役常驻 worker |
| `fleet` / `tail` | 舰队概览（每个会话及其判定）、单个会话的判定与事件尾随 |
| `version` | 打印 `spawn <版本>` |

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `SPAWN_DATABASE_URL` | 必填 | PG 连接串 |
| `SPAWN_GIT_REPO` | 必填 | 不属于任何任务的 agent（supervisor、临时 worker）用的裸仓库 |
| `SPAWN_WORKSPACE_ROOT` | 必填 | 每个分支一份工作树的根目录；启动时解析符号链接一次，工作区 id 都在解析后的根下 |
| `SPAWN_CLAUDE_PROGRAM` | `claude` | Claude 可执行文件 |
| `SPAWN_CLAUDE_PROJECTS` | `$CLAUDE_CONFIG_DIR/projects`，否则 `~/.claude/projects` | Claude 写 transcript 的目录 |
| `SPAWN_MAX_FILE_MB` | `100` | checkpoint 收录的单个文件上限（MiB）；更大的文件不进 checkpoint，并告知 agent 一次 |
| `SPAWN_TMUX_PROGRAM` | `tmux` | tmux 可执行文件 |
| `SPAWN_TMUX_SOCKET` | 无 | 私有 tmux socket |
| `SPAWN_JUDGE_URL` | 未设 | 判定服务根地址，未设时用内置规则 |
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
| `SPAWN_STALL_GATE_SECS` | `3600` | 带门禁的任务等集成器判定多久就升级：门禁自己的墙钟是 45 分钟 |
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

检查项都是 `rsi/scenarios/check-*.sh` 场景，见 [fleet-architecture.md](./fleet-architecture.md) §10.2。

## 9. 依赖约束

不使用 async 运行时，不使用 chrono、uuid、rand，不使用 HTTP 客户端库。时间统一为 `i64` 毫秒。`spawn-jev` 的 HTTP/1.1 客户端由标准库 socket 手写，支持 `Transfer-Encoding: chunked`。
