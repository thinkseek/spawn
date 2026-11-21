# Spawn Feature Map

## 读法与证据边界

代码与行号基线为提交 `c17110c5f7d94edf6bb1a50c5643d9a167527ea2`。Linux 验证机上的测试目录与该提交的补丁 SHA-256 均为 `4afdb198826733dde90bce7176ea635c2de8ec7e3f15caa12943a3940d620815`。后续文档提交只更新本图与校验脚本。后续代码修改必须同步更新引用。

- 实现状态只使用 `已实现`、`部分实现`、`未实现`、`已知缺陷`。
- 部署状态只使用 `部署未验证`、`部署已验证`、`未部署`、`不适用`。本次没有执行部署，也没有读取目标运行环境中的进程、数据库或日志，因此没有把源码存在写成线上已部署。
- 测试证据给出测试源码位置。它不等于本次测试通过。每次回写必须另外记录实际运行的命令、环境和结果。
- 项目运行数据以 PostgreSQL 事件、主机归档和运行记录各自的事实来源为准。后训练文档以其所属项目的事实来源为准。本图只连接这些边界，不复制其数据，不把机器私有路径写进公开代码仓。
- 本图从 `cdb0fbdc` 恢复到无共同祖先的新主干。FM-01 至 FM-09、FM-11 至 FM-17 的实现叙述和历史测试结果继承旧图；本次只核对并更新其源码引用，不把旧提交的运行结果写成新基线结果。FM-10、FM-18、FM-19 记录本次实际验证。
- FM-18 和 FM-19 的代码缺陷已在上述基线修复。运行状态不能证明代码已修复；代码修复也不能证明某个环境已经部署。

## 核心编排与 PostgreSQL

### FM-01 Mission 与 task 控制流

- 能力：创建和配置 mission，记录 owner push、hold/release，驱动 task 状态及 gate verdict 折叠。
- 入口：`spawn mission ...`、`spawn task ...`、SQL control-record 写入与 refold。
- 代码：`crates/spawn-cli/src/main.rs:135-137`、`crates/spawn-cli/src/missions.rs:603-780`、`crates/spawn-core/src/mission.rs:14`、`crates/spawn-postgres/sql/schema.sql:2553`、`crates/spawn-postgres/sql/schema.sql:2631`。
- 实现状态：`已实现`。配置和任务变化先成为记录，SQL 再折叠为读模型；`aborted` 本身不把 task 推进到完成态。
- 测试证据：`crates/spawn-core/src/mission.rs:87-125` 固定状态值和未知值错误；`crates/spawn-postgres/tests/control.rs:51-135` 覆盖 mission/resident 控制记录折叠。本次在 Darwin/aarch64 运行 `cargo test -p spawn-core mission::tests`，2 个测试通过。
- 部署状态：`部署未验证`。需要同一套 CLI、schema 和 loop；本次未执行端到端 mission 生命周期。
- 已知缺口：accepted 后的 landing/landed 依赖 integrator；仅有控制命令成功不能证明工作已经落地。
- 下一步：在一次性数据库中覆盖 assigned→reported→accepted→landing→landed，并分别保留 aborted、stale、conflict 的证据。

### FM-02 Objective 调度、dispatcher 与 stall 升级

- 能力：从 backlog 分配 objective，处理 attempt/yield、worker slots、continuation，重复生成由记录决定的投递计划，并升级停滞。
- 入口：`spawn mission objectives ...`、`spawn integrate` 分配 objective；`spawn loop` 运行 dispatcher 和 stall；核心函数为 `objectives::assign_next`、`dispatch::decide_with`。
- 代码：`crates/spawn-cli/src/objectives.rs:1000-1086`、`crates/spawn-cli/src/integrate.rs:3573-3591`、`crates/spawn-cli/src/dispatch.rs:623-675`、`crates/spawn-cli/src/commands/loops.rs:415-473`、`crates/spawn-cli/src/dispatch/stall.rs:1-50`。
- 实现状态：`已实现`。dispatcher 由记录和时钟生成计划；stall 生成 supervisor 通知，不直接修复阻塞。
- 测试证据：`crates/spawn-cli/src/objectives.rs:1670-1732` 覆盖 slot 与 yield；`crates/spawn-cli/src/dispatch.rs:1404` 覆盖同事实重复决策；`crates/spawn-cli/src/dispatch/stall/tests.rs:133-174` 覆盖停滞计时起点。
- 部署状态：`部署未验证`。backlog 和 scenarios 是 controller 输入；本次未核对部署副本或真实 worker。
- 已知缺口：backlog JSON 与 scenario 文件不是 PostgreSQL 内的任务事实；部署时需要额外保证它们与目标 revision 一致。
- 下一步：运行 scheduler、deterministic dispatch 和 stall 测试，再核对 deployed controller 的 backlog、scenarios 与目标 revision。

### FM-03 PostgreSQL 追加事件日志

- 能力：脱敏后追加事件，按 source key 幂等，维护稠密 session 序号、全局顺序、单 open execution 和可重建 projection。
- 入口：`EventLog::{append, read_global, open_execution}`、SQL `spawn_append`、`spawn_append_trusted`。
- 代码：`crates/spawn-core/src/port.rs:58-74`、`crates/spawn-postgres/src/event_log.rs:58-110`、`crates/spawn-postgres/sql/schema.sql:257-394`。
- 实现状态：`已实现`。写入持事务级锁；重复 source key 返回原行；事件与 session projection 在同一 SQL 调用中更新。
- 测试证据：`crates/spawn-core/src/contract.rs:42-117` 是共享一致性契约；`crates/spawn-postgres/tests/api.rs:72`、`crates/spawn-postgres/tests/api.rs:80`、`crates/spawn-postgres/tests/api.rs:95` 覆盖 PostgreSQL 契约、幂等与序号；`crates/spawn-postgres/tests/api.rs:1126-1182` 覆盖 projection 重建。
- 部署状态：`部署未验证`。本次未连接数据库，也未测量并发写入或 projection 漂移修复。
- 已知缺口：顺序和 projection 保证要求写方经过 SQL 函数；直接写表不在契约内。
- 下一步：在一次性数据库中运行共享契约、并发 append 和强制 stale projection 测试。

### FM-04 幂等命令队列与事件唤醒

- 能力：幂等 submit、单 session 串行 lease、waiting-input 让行、release/finish、watermark、LISTEN/NOTIFY 和定时回退。
- 入口：`CommandQueue::{submit, claim, release, finish}`、`PostgresStore::{watermark, listen, next_lease_ms}`。
- 代码：`crates/spawn-postgres/src/command_queue.rs:21-67`、`crates/spawn-postgres/sql/schema.sql:396-600`、`crates/spawn-postgres/src/wake.rs:34-86`、`crates/spawn-cli/src/commands/loops.rs:539-595`。
- 实现状态：`已实现`。submit 与 `command.accepted` 同事务；finish 与 `command.finished` 同事务；丢失通知由 watermark 和到期时间兜底。
- 测试证据：`crates/spawn-postgres/tests/api.rs:117-267` 覆盖幂等与串行竞争；`crates/spawn-postgres/tests/waiting.rs:67-129` 覆盖 waiting 与 lease；`crates/spawn-postgres/src/wake.rs:131-211` 覆盖通知和到期调度。
- 部署状态：`部署未验证`。本次未运行 submit→claim→finish，也未断开 LISTEN 连接。
- 已知缺口：通知不代表每个事件都会触发所有重步骤；screen/transcript 仍按轮询读取。
- 下一步：运行 queue 和 wake 测试，在 staging 断开 listener，确认定时 watermark 继续且不形成紧密循环。

### FM-05 Schema 迁移、受限角色与恢复事实

- 能力：显式 `init-db` 原地迁移，回填 projection，提供 per-database 受限登录和持久化 recovery facts。
- 入口：`spawn init-db`、`PostgresStore::migrate`、`role_url`、`spawn_host_record`。
- 代码：`crates/spawn-cli/src/main.rs:115-118`、`crates/spawn-postgres/src/lib.rs:284-348`、`crates/spawn-postgres/sql/schema.sql:129-200`、`crates/spawn-postgres/src/gateway.rs:178-313`、`crates/spawn-postgres/src/lib.rs:487-519`、`crates/spawn-postgres/src/recovery.rs:27-106`、`crates/spawn-postgres/sql/schema.sql:5323-5366`。
- 实现状态：`已实现`。普通 `open_store` 只连接；迁移是独立部署步骤。受限登录由 owner 密码派生，recovery writer 只接受 owner。
- 测试证据：`crates/spawn-postgres/tests/upgrade.rs:91` 覆盖旧表升级；`crates/spawn-postgres/tests/api.rs:2975-3025` 覆盖迁移竞争；`crates/spawn-postgres/tests/roles.rs:108-149` 覆盖数据库间角色隔离；`crates/spawn-postgres/tests/recovery.rs:56`、`crates/spawn-postgres/tests/recovery.rs:117` 覆盖 recovery writer。
- 部署状态：`部署未验证`。本次未执行 init-db、角色连接或数据库重启。
- 已知缺口：每次 schema application 都会对整个事件日志做分组回填；大日志版本化优化尚未实现。组合 recovery 读 API 缺少直接命名的集成测试。
- 下一步：在代表性副本上测量迁移并增加版本门控；补 recovery_sessions/signals/facts 的一次性数据库测试。

## Resident、worktree 与 Claude Code

### FM-06 Resident 审批、退休、保活与恢复

- 能力：propose/approve/retire resident，维持已批准 worker，按持久化恢复事实选择 resume、wait、handoff 或 final。
- 入口：`spawn resident propose|approve|retire`、`supervise::keep_residents`、loop recovery step。
- 代码：`crates/spawn-cli/src/missions.rs:1038-1057`、`crates/spawn-cli/src/supervise.rs:358-437`、`crates/spawn-cli/src/commands/loops.rs:398-473`、`crates/spawn-cli/src/recover.rs:1169-1248`。
- 实现状态：`已实现`。retire 会进入 open-task 取消路径；resident 后续投递还受 FM-18 的通知顺序约束。
- 测试证据：`crates/spawn-postgres/tests/control.rs:51-80` 覆盖 propose/approve/retire；`crates/spawn-cli/src/supervise.rs:630-672` 覆盖 revival backoff 与 handoff；`crates/spawn-cli/src/recover.rs:2933`、`crates/spawn-cli/src/recover.rs:2972` 覆盖恢复退避和 handoff/final。
- 部署状态：`部署未验证`。本次未进行 owner 审批、退休、进程重启或恢复演练。
- 已知缺口：新的 native session 不自动继承旧对话上下文；恢复依赖 workspace、transcript、report 和记录。
- 下一步：在隔离环境覆盖 propose→approve→retire、resume/handoff，并与 cancel-notice 场景一起验证。

### FM-07 分支导出、worktree、checkpoint 与 set-aside

- 能力：从 agent 分支导出无 `.git` 的工作目录，做 checkpoint CAS、main merge、冲突续作和取消工作的 set-aside。
- 入口：`GitWorkspace`/`WorkspaceStore` 的 branch、mount、checkpoint、merge、set-aside。
- 代码：`crates/spawn-host/src/git.rs:54-116`、`crates/spawn-host/src/git.rs:1163-1417`、`crates/spawn-cli/src/set_aside.rs:77-154`、`crates/spawn-cli/src/main_merge.rs:1`。
- 实现状态：`已实现`。这里的“worktree”是 `git archive` 式 plain export，不是带 `.git` 的常规 `git worktree`；分支头保存快照。
- 测试证据：`crates/spawn-host/tests/api.rs:123-195` 覆盖真实 git branch/mount/checkpoint；`crates/spawn-host/src/git.rs:3126-3218` 覆盖大文件边界；`crates/spawn-cli/src/set_aside.rs:225-240` 覆盖取消通知内容。
- 部署状态：`部署未验证`。本次未创建真实 agent workspace 或合并冲突。
- 已知缺口：默认单文件 100 MiB checkpoint 上限不适合大数据集和模型产物；超限文件需要独立 artifact 存储。
- 下一步：运行 real-git、main-merge、set-aside 测试，并为大型数据产物定义独立保留与血缘方案。

### FM-08 tmux 与 Claude Code 执行生命周期

- 能力：建立精确 tmux session，启动 Claude Code runner，分段输入长 prompt，处理 trust dialog、exit、lost runner 和终止清理。
- 入口：`spawn run -- claude ...`、`Bridge::type_prompt`、`TmuxHost::{start, stage_text, runner, terminate}`。
- 代码：`crates/spawn-core/src/bridge.rs:4432-4532`、`crates/spawn-host/src/tmux.rs:590-752`、`crates/spawn-cli/src/commands/engine.rs:24-45`、`crates/spawn-host/src/tmux.rs:831-943`。
- 实现状态：`已实现`。runner 写退出状态；Bridge 结合 transcript、pane 和 runner 事实判定 exited/lost/stopped。
- 测试证据：`crates/spawn-host/tests/api.rs:537-569` 覆盖长文本；`crates/spawn-host/tests/api.rs:571-645` 覆盖环境不入 argv；`crates/spawn-host/tests/runner.rs:183` 覆盖 runner 身份；`crates/spawn-host/tests/runner.rs:415`、`crates/spawn-host/tests/runner.rs:501` 覆盖终止等待；`crates/spawn-core/tests/waiting_inputs.rs:343` 覆盖 ready 后输入。
- 部署状态：`部署未验证`。本次未启动 tmux 或 Claude Code。
- 已知缺口：权限绕过配置要求外层 sandbox；`setsid` 离开记录 session 的后代不在现有等待/SIGKILL 集合内。非 Linux 也没有等价 `/proc` 清理保证。
- 下一步：在目标 Linux 用隔离 tmux socket 做启动、长 prompt、外部 kill 和脱离 session 的故障演练。

### FM-09 Transcript、screen、归档与 Observer

- 能力：增量归一化 Claude Code JSONL 与 pane 屏尾，保存 append-only transcript copies，规则优先并在歧义时调用 Jev。
- 入口：`claude_line`、`read_claude_screen`、`TranscriptArchive`、`Observer::tick`、`JevJudge::judge`。
- 代码：`crates/spawn-core/src/ingest.rs:917-1009`、`crates/spawn-core/src/screen.rs:355-409`、`crates/spawn-host/src/transcripts.rs:128-224`、`crates/spawn-core/src/observe/observer.rs:294-370`、`crates/spawn-jev/src/lib.rs:198-221`、`crates/spawn-host/src/watch.rs:161-175`、`crates/spawn-host/src/watch.rs:297-312`。
- 实现状态：`部分实现`。Linux 使用 inotify；当前非 Linux `unwatch` 仍调用仅 Linux 定义的 `release`，旧图中的无适配器回退不存在。
- 测试证据：`crates/spawn-core/src/ingest.rs:1308-1396` 覆盖事件归一与未知行；`crates/spawn-core/src/transcripts.rs:857-871`、`crates/spawn-core/src/transcripts.rs:978-1012`、`crates/spawn-core/src/transcripts.rs:1029-1064`、`crates/spawn-core/src/transcripts.rs:1248-1276` 覆盖归档和恢复；`crates/spawn-core/tests/observe_tiers.rs:180-338` 覆盖规则/Jev 分层；`crates/spawn-host/src/watch.rs:344-345` 只有 `cfg(test)`，没有旧图所述的 Linux-only 门禁。旧提交 `41ece56e` 的 Darwin 构建与 Linux 单元测试结果是历史证据；当前 Darwin/aarch64 的 `cargo test -p spawn-cli deliver::tests` 在 `spawn-host` 的 Linux-only 代码编译失败，不记为通过。
- 部署状态：`部署未验证`。本次未读取真实 transcript、归档或 Jev relay。
- 已知缺口：解析依赖 Claude Code 版本格式。归档在 turn end 和 execution end 触发，不是连续复制。本地归档不提供跨主机容灾；非 Linux 没有 inotify 或等价 `/proc` 清理保证，且当前 `spawn-host` 存在平台编译缺口。
- 下一步：引擎升级时新增真实 fixtures，并在目标环境演练 rewrite/delete/resume 与 loopback Jev 故障；需要非 Linux 支持时先修复平台门禁。

## Gate、integrator 与 fleet

### FM-10 黑盒 gate 与 sandbox

- 能力：用 controller scenarios 判定 base/candidate，保护路径，复用 base cache，在 bubblewrap、离线 Cargo 和受限网络中运行候选代码。
- 入口：`scripts/rsi/gate.sh <repo> <base> <candidate> <scenarios> [scenario]`、`gate::judge`。
- 代码：`scripts/rsi/gate.sh:5-52`、`scripts/rsi/gate.sh:128-149`、`scripts/rsi/gate.sh:633-650`、`crates/spawn-cli/src/gate.rs:1078-1167`。
- 实现状态：`已实现`。可信 verdict 来自私有文件且要求 gate 成功退出；host fault、no-room、cannot-judge 与 candidate rejection 分开。
- 测试证据：`scripts/rsi/test-gate.sh:157-283` 覆盖基本判定、revision 和非 UTF-8 locale；`scripts/rsi/test-gate.sh:2989-3108` 覆盖 base-only 与缓存；`scripts/rsi/test-gate.sh:3958-3975`、`scripts/rsi/test-gate.sh:3976-4047` 是 PostgreSQL toolchain/server 与 schema upgrade 条件路径。代码提交 `c17110c5` 在 Linux/x86_64、Git 2.56.1、Rust 1.95.0、bubblewrap 0.13.1、tmux 3.8、PostgreSQL 19.0 下运行 `LC_ALL=C TMPDIR="$tmp" sh scripts/rsi/test-gate.sh`（`tmp=spawn-fm18-fm19-main-5e416443/gate-tmp-ordinary-r2`），790 个通过、0 失败、2 个跳过，退出状态 0，日志 `test-gate-ordinary-r2.log`；host-programs 扩展组和无本地镜像的 Docker 分支跳过，两个 PostgreSQL 条件路径只打印 note。随后在 Linux 验证机上的同一证据目录（`evidence=spawn-fm18-fm19-main-5e416443`）以真实 `gate.sh` 分别运行独立场景：`sh "$evidence/pg-toolchain-scenario-r3.sh"` 为 5 个断言通过，日志 `pg-toolchain-scenario-r3.log` 含 `psql 19.0 server 190000`；`sh "$evidence/pg-schema-scenario-r4.sh"` 为 10 个断言通过，日志 `pg-schema-scenario-r4.log`。这些退出状态分别记录，不声称普通套件与条件路径的组合套件通过。
- 部署状态：`部署未验证`。Linux 验证机只运行用户态工具链、一次性 bubblewrap、Git、tmux 和 PostgreSQL 测试，没有部署或重启现有服务。
- 已知缺口：base cache 仍有 same-UID 信任限制；空 revision 的入口校验及证据单列在 FM-19。host-programs 扩展组、Docker image 分支及组合套件的条件路径仍缺一次共同通过记录。
- 下一步：在标准 PostgreSQL 路径下复跑组合套件，并在有 `postgres:16-alpine` 镜像时补 Docker 分支；需要抵御同 UID 恶意进程时，把 cache 与执行放进独立 UID 边界。

### FM-11 Integrator freshness、verdict 与 CAS landing

- 能力：核对 published head 与 report checkpoint，构造两父 landing commit，绑定 verdict revision，以 CAS 推送 main，并处理 owner push。
- 入口：`spawn integrate`、report freshness、`landing_candidate`、`land`、`integrate/owner.rs`。
- 代码：`crates/spawn-cli/src/integrate.rs:1143-1225`、`crates/spawn-cli/src/integrate/landing.rs:17-120`、`crates/spawn-cli/src/integrate/landing.rs:280-418`、`crates/spawn-cli/src/integrate/owner.rs:1-60`、`crates/spawn-cli/src/integrate.rs:999-1015`。
- 实现状态：`已实现`。连续 abort 由 `halt_reason` 计数；push 与记录之间仍需要恢复路径，进程内 judging 完成不等于 landed。
- 测试证据：`crates/spawn-cli/src/integrate/tests.rs:825-937` 覆盖 freshness；`crates/spawn-cli/src/integrate/tests.rs:2079-2180` 覆盖两父 landing；`crates/spawn-cli/src/integrate/database_tests.rs:2945-3026` 覆盖 revision-bound verdict；`crates/spawn-cli/src/integrate/database_tests.rs:4437-4508` 覆盖 owner push 顺序。
- 部署状态：`部署未验证`。本次未执行 gate、landing 或 push。
- 已知缺口：真实结果依赖 remote、lease、base 与中断恢复；remote shadow verdict 当前不能决定 landing。
- 下一步：运行 freshness、CAS、中断补记、incompatible verdict 和 owner ordering 测试，再在 staged remote 验证并发 base 变化。

### FM-12 Fleet、alerts、serve 与人工输入

- 能力：SQL fleet/tail 读模型、alert fire/clear/notice、attention 计划、dashboard reader，以及受限 human input/key 写入。
- 入口：`spawn fleet|tail|alerts`、`spawn serve`、`/api/attention`、SQL gateway。
- 代码：`crates/spawn-postgres/src/fleet.rs:12-109`、`crates/spawn-cli/src/alerts.rs:988-1081`、`crates/spawn-cli/src/serve/attention.rs:157-244`、`crates/spawn-postgres/src/gateway.rs:60-169`。
- 实现状态：`已实现`。reader 与 human-write 使用不同权限路径；attention 是瞬时 dispatcher plan，不提交命令。
- 测试证据：`crates/spawn-postgres/tests/dashboard.rs:73-179` 覆盖 board；`crates/spawn-postgres/tests/alerts.rs:60-128` 覆盖 alert 生命周期；`crates/spawn-cli/src/serve/attention.rs:1049-1088` 覆盖 attention parity；`crates/spawn-postgres/tests/human_input.rs:137-228`、`crates/spawn-postgres/tests/human_input.rs:394-455` 覆盖人工输入与角色限制；`crates/spawn-postgres/tests/human_key.rs:117-187`、`crates/spawn-postgres/tests/human_key.rs:230-293` 覆盖 key 命令与权限。
- 部署状态：`部署未验证`。本次未启动 HTTP 服务、查询 live fleet 或验证 token/Origin/network ACL。
- 已知缺口：fleet board 不是进程健康证明；没有 backup record 本身不会触发 backup-age alert；采样和投影延迟会影响新鲜度。
- 下一步：运行 dashboard、alerts、attention 和 HTTP guard 测试，并用一次性数据验证 reader 不能写、human role 只能输入。

## 部署与数据连接

### FM-13 Gated deploy、不可变 controller 与 systemd

- 能力：验证 accepted verdict，构建和安装 artifact，迁移 schema，切换 controller，移动 main，重启 loop/integrate/serve/relay。
- 入口：`deploy/deploy.sh`、`deploy/controller.sh`、`deploy/cutover-controller.sh`、`deploy/systemd/*.service`。
- 代码：`deploy/deploy.sh:544-653`、`deploy/deploy.sh:655-777`、`deploy/controller.sh:179-196`、`deploy/controller.sh:208-273`、`deploy/controller.sh:329-373`、`deploy/controller.sh:377-404`、`deploy/systemd/spawn-loop.service:1-30`、`deploy/systemd/spawn-integrate.service:1-26`。
- 实现状态：`已实现`。部署脚本有恢复记录和 fake-live 测试；实际部署仍必须独立记录 commit、环境和 unit 健康。controller 的 `web/node_modules` 复制在文件系统支持时走 reflink，不支持时走真实全量复制，两条路径都产生独立 inode；硬链接回退已移除，因为共享 inode 会让源目录的写入到达只读 controller。同一语义在 `crates/spawn-cli/src/integrate/owner.rs:877-895` 的候选树复制中保持一致。
- 测试证据：`deploy/test-deploy.sh:348-414` 覆盖 pending 恢复；`deploy/test-deploy.sh:596-789` 覆盖 drain/timeout；`deploy/test-controller.sh:72-113` 覆盖构建副本、inode 独立性和强制无 reflink 的全量复制回退；`deploy/test-controller.sh:125-267` 覆盖原子切换和保留；`crates/spawn-cli/src/integrate/owner.rs:2132-2180` 断言候选树复制得到独立 inode 且对副本的写不触及 controller。Linux/x86_64、Debian 12、ext4（`cp --reflink=always` 返回 Operation not supported）、Rust 1.95.0、git 2.43.0、bash 5.2.15、mawk 1.3.4（在 `unshare -rm` 命名空间内绑定为 awk）、一次性 PostgreSQL 16.4 集群（独立 socket 与 data 目录，trust 认证）、独立临时 checkout/target/logs，于 2026-11-21 运行：`cargo test -p spawn-cli` 三个测试二进制共 1236 通过、0 失败、3 忽略（1234+1+1，含新增的 inode 独立性测试）；`cargo test -p spawn-host` 共 151 通过、0 失败、1 忽略；`bash deploy/test-controller.sh` 为 79 ok、0 失败；`bash deploy/test-session-backup.sh` 为 124 ok、0 失败；`python3 -I scripts/check-feature-map.py` 为 19 条目 177 引用 OK，`--self-test` 7 个负例 OK，`git diff --check` 无输出，公司标记/`.internal`/私有网段 IP/个人标识扫描全部 none。这些是隔离临时目录的结果，不连接任何现有服务。
- 部署状态：`部署未验证`。本次只读脚本，没有运行 deploy、systemctl 或健康检查。
- 已知缺口：写入新事件后的旧二进制回滚受版本兼容约束；cutover 含主机特定配置和无界 gate 等待。既有 engine 在 restart/resume 前保留旧环境。
- 下一步：在 GNU/Linux 隔离环境运行 fake-live deploy/controller 测试；真实部署后记录 installed commit、unit 状态、HTTP/DB 检查和新 engine 环境。

### FM-14 远程 gate runner 与 shadow judging

- 能力：打包两个 revision 和 controller，在隔离 runner 执行，校验 MANIFEST/verdict commits，收集结果，并做非权威 shadow 对比。
- 入口：`deploy/gate-runner/setup.sh`、`deploy/gate-runner.sh [--check]`、`SPAWN_GATE_SHADOW_RUNNER`。
- 代码：`deploy/gate-runner.sh:166-243`、`deploy/gate-runner.sh:675-831`、`deploy/gate-runner/remote.sh:340-446`、`crates/spawn-cli/src/integrate/shadow.rs:320-345`。
- 实现状态：`已实现`。远程结果只比较和记录；shadow 不决定 landing、halt、计数或预算。
- 测试证据：`deploy/test-gate-runner.sh:272-435` 覆盖成功、超时、传输重试和伪造 verdict；`deploy/test-gate-runner.sh:505-628`、`deploy/test-gate-runner.sh:788-793` 覆盖资源与隔离；`crates/spawn-cli/src/integrate/database_tests.rs:8874-9023` 覆盖 shadow 不做决定。
- 部署状态：`部署未验证`。本次未连接 SSH runner、私有数据库或远程 sandbox。
- 已知缺口：单个 in-flight shadow 且 offer 可跳过，不能形成完整 verdict 样本；fake runner 不证明真实 toolchain 兼容。
- 下一步：在 GNU/Linux runner 上执行自测和 `--check`；保持非权威，直到 revision-bound agreement 数据支持另行评审的授权设计。

### FM-15 例行清理与 maintenance timer

- 能力：按各自的名字、owner 和年龄清理 gate scratch、base cache 与私有 PostgreSQL templates；按精确文件名保留最新 N 份 deploy dump。
- 入口：`scripts/maintenance.sh`、`spawn-maintenance.service`、`.timer`、dry-run。
- 代码：`scripts/maintenance.sh:59-169`、`scripts/maintenance.sh:171-225`、`deploy/systemd/spawn-maintenance.service:5-24`、`deploy/systemd/spawn-maintenance.timer:4-13`。
- 实现状态：`已实现`。每一步独立报告失败；dry-run 只打印计划，不删除。
- 测试证据：`scripts/test-maintenance.sh:151-230` 覆盖 dry-run、保留、symlink、安全期限、幂等和独立失败。
- 部署状态：`部署未验证`。本次未执行清理或读取 timer 日志。
- 已知缺口：脚本和测试依赖 GNU find/touch/date；真实 timer 的首次运行和保留清单仍未演练。当前实现按 owner 记录、进程使用和 gate wall-clock 保护活跃 scratch。
- 下一步：在目标 GNU/Linux 先跑测试和 dry-run，逐项审查路径后再启用 timer 并检查首次真实运行。

### FM-16 加密备份与恢复演练

- 能力：加密 PostgreSQL dump、source/transcript bundle，执行上传预算检查，提供 R2 lock/lifecycle 保留配置，并在独立数据库验证恢复。
- 入口：`spawn backup dump|bundle|verify`、backup systemd timers；旧 shell 工具只作手工 legacy 路径。
- 代码：`crates/spawn-cli/src/backup.rs:1157-1359`、`crates/spawn-cli/src/backup.rs:1456-1569`、`crates/spawn-cli/src/backup.rs:2703-2848`、`deploy/systemd/spawn-backup-verify.service:5-24`。
- 实现状态：`已实现`。verify 对比事件数和 high-watermark，并清理独立 drill database；R2 lock/lifecycle 文件只是配置输入。
- 测试证据：`crates/spawn-cli/src/backup.rs:3861-3978` 覆盖 tiers、limits、bundle 和 suspect dump；`crates/spawn-postgres/tests/backup.rs:152-168` 覆盖记录权限；`deploy/backup/test-pg-restore-drill.sh:72-146` 只覆盖 legacy Docker drill。
- 部署状态：`部署未验证`。本次未访问 R2、密钥、真实备份或 drill server。
- 已知缺口：本地 stand-in 不证明 Cloudflare 权限或 object lock；verify 需要独立 CREATEDB 能力，密钥托管需要单独核验。
- 下一步：在一次性环境运行 backup tests 和真实恢复，独立读回 R2 规则，并记录 dump/bundle/verified 事实及恢复结果。

### FM-17 数据工程与后训练连接边界

- 能力：提供 SQL operational views、scorecard JSON、原始 transcript 归档、加密 bundle、objective METRIC，以及 Supply PA 依赖 store 和可选测试数据库输入。
- 入口：`spawn scorecard --json`、`spawn backup bundle`、`SUPPLY_PA_DEPS`、gate `METRIC`、`no-test-database`。
- 代码：`crates/spawn-postgres/sql/schema.sql:611-921`、`crates/spawn-cli/src/scorecard.rs:132-204`、`crates/spawn-host/src/transcripts.rs:128-224`、`crates/spawn-cli/src/backup.rs:2186-2586`、`crates/spawn-cli/src/runtime.rs:136-176`、`crates/spawn-cli/src/objectives.rs:171-216`、`crates/spawn-cli/src/gate.rs:96-132`。
- 实现状态：`部分实现`。运行数据采集、分析、备份和外部项目依赖输入已存在；未找到直接的 post-training、dataset export 或标注平台连接器。
- 测试证据：`crates/spawn-cli/src/scorecard/tests.rs:878-997` 覆盖 token 去重和 mission 归属；`crates/spawn-cli/src/runtime.rs:401-416` 覆盖依赖路径注入；`crates/spawn-host/src/transcripts.rs:325-346` 覆盖本地归档契约。
- 部署状态：`部署未验证`。本次未连接外部数据工程系统、Supply PA controller、在线 relay 或后训练平台。
- 已知缺口：SQL views 和 transcript 不是版本化训练集；原始内容可能含秘密。`SUPPLY_PA_DEPS` 只传依赖路径，在线 model relay 也不是训练管道。
- 下一步：如需要后训练连接，另定义版本化 export schema、脱敏、去重、血缘和删除策略；运行数据与后训练文档继续各守事实来源。

## 已修缺陷

### FM-18 未应答 cancel-notice 阻塞 resident 后续投递

- 能力：取消通知的顺序保证及其对同一 resident session 后续任务的影响。
- 入口：`CANCELLED_DELIVERIES` → `Work::blocked` → `next_up`/`deliver::orders`。
- 代码：`crates/spawn-postgres/src/missions.rs:344-385`、`crates/spawn-postgres/sql/schema.sql:3792-3811`、`crates/spawn-cli/src/deliver.rs:769-776`、`crates/spawn-cli/src/deliver.rs:1475-1482`、`crates/spawn-cli/src/dispatch.rs:999-1010`。
- 实现状态：`已实现`。匹配的 cancel-notice 命令落盘即为 `told=true`，不再要求 `succeeded`；work stealing 使用同一 recorded 语义。首次 notice 前的 `told=false` 仍保证 stop→set-aside→notice；open command 仍串行化输入，failed 或 dispatcher restart 不再阻塞后续任务，也不会重复 notice。事件格式未变。
- 测试证据：`crates/spawn-postgres/tests/cancelled.rs:91-168` 分别覆盖 submitted、succeeded=true、succeeded=false；`crates/spawn-postgres/tests/steal.rs:328-363` 覆盖 submitted/failed notice 与 idle 判定；`crates/spawn-cli/src/deliver.rs:3640-3685` 覆盖 Assigned/Revise、failed/restart 后空闲 pane 直接 Input 以及 open 时不并发输入。`5e416443` 基线在 Darwin/aarch64、PostgreSQL 18 运行两个数据库回归，cancelled 在 submitted 断言得到 `left: Bool(false), right: true`，steal 在 `succeeded=None` 断言失败，均为 0 通过、1 失败；修复后同一命令各 1 个通过。基线的 idle-pane planner 新回归已 1 个通过，所以 planner 生产代码零移植。Linux/x86_64、Rust 1.95.0、PostgreSQL 19 上运行 `cargo test -p spawn-cli deliver::tests -- --test-threads=1`，63 个通过；`cargo test -p spawn-postgres --test cancelled -- --test-threads=1`，2 个通过；`cargo test -p spawn-postgres --test steal -- --test-threads=1`，4 个通过。最终源码在同一 Linux 环境运行 `cargo test --workspace -- --test-threads=1`，73 个 test binaries 加 5 组 doc-tests 共 2028 个通过、0 失败、5 忽略，日志 `cargo-workspace-candidate-r5.log`，退出状态 0。
- 部署状态：`部署未验证`。本次只做源码、一次性数据库和纯函数验证，未操作真实 resident 或现有服务；目标运行环境是否未修复或已经部署，本次均未核实。
- 已知缺口：未做真实 tmux/Claude Code pane 的 open→failed→restart 端到端演练。公开议题 [#76](https://github.com/thinkseek/spawn/issues/76) 的 dispatcher cancel-stop 被 recovery 记为 final 是另一条路径，本项不声称修复它。
- 下一步：在隔离的部署演练中核对真实 queue 与 pane 的 open/failed/restart，并单独验证 #76 的 recovery 路径。

### FM-19 空 base/candidate 造成无进展 gate 空转

- 能力：normal gate mode 的 revision 输入校验，以及空 base 与空 candidate 在 integrator 中的不同分类。
- 入口：`gate.sh` 参数解析、`git rev-parse --verify`、`gate::judge_seeing_base_check` → `judge_in` → `decide`、`task.gated` fold。
- 代码：`scripts/rsi/gate.sh:633-650`、`crates/spawn-cli/src/gate.rs:1078-1089`、`crates/spawn-cli/src/gate.rs:1158-1167`、`crates/spawn-cli/src/integrate.rs:4050-4076`、`crates/spawn-cli/src/integrate.rs:4125-4180`、`crates/spawn-cli/src/integrate.rs:4238-4256`、`crates/spawn-cli/src/integrate.rs:4347-4410`、`crates/spawn-cli/src/integrate/landing.rs:17-27`、`crates/spawn-cli/src/integrate/verdicts.rs:1003-1018`、`crates/spawn-cli/src/integrate/owner.rs:1807-1864`、`crates/spawn-postgres/sql/schema.sql:2631-2635`。
- 实现状态：`已实现`。standalone 在 bwrap/scratch/rev-parse 前对完整 Rust trim 空白集返回专用 exit 64。Rust 在创建私有目录或启动 gate 前返回 `GateFailure::Parameter`。生产 base 入口、normal/adopted candidate 和 stale 检查都记录 `aborted` 加 `evidence=parameter_error`，由现有连续 abort 规则限次；candidate 与 main 前进竞态也不会改写为 stale。合法 nonexistent ref 仍走 rev-parse exit 3 和 `Aborted`。
- 测试证据：`scripts/rsi/test-gate.sh:220-283` 覆盖 nonexistent ref、empty + 25 个 Rust 空白字符的 52 个 standalone 输入和非 UTF-8 locale；`crates/spawn-cli/src/gate.rs:3010-3058` 覆盖 52 个 Rust 参数错误、gate 未执行和 nonexistent-ref 兼容；`crates/spawn-cli/src/integrate/tests.rs:1219-1267` 覆盖 stale 防护与序列化；`crates/spawn-cli/src/integrate/database_tests.rs:3454-3548` 覆盖 empty/space/U+3000 的 base/candidate 持久化、abort 计数和 main 前进竞态；`crates/spawn-cli/src/integrate/database_tests.rs:5760-5790` 覆盖 owner repeat 分类。`5e416443` 基线的 empty/space/tab/newline 输入均返回 exit 3；修复后 Darwin/aarch64 上 52/52 返回 exit 64，nonexistent ref 为 exit 3。审查发现 `zh_CN.GB18030` 的 NBSP 曾落到 exit 3；固定 `tr` 和 `grep` 为 C locale 后，GB18030 的 52 个输入全部返回 exit 64，6 个非 UTF-8 locale 的 NBSP/U+3000 两侧共 24/24 返回 exit 64。Linux/x86_64、Rust 1.95.0、PostgreSQL 19 上，上述 Rust gate、stale、serialization、empty base、empty candidate 和 repeat 六个命令各 1 个测试通过。竞态回归先得到 `verdict=stale`、0 通过、1 失败，修复后 base/candidate 两个命令各 1 个通过。最终源码运行 `cargo test --workspace -- --test-threads=1`，73 个 test binaries 加 5 组 doc-tests 共 2028 个通过、0 失败、5 忽略，日志 `cargo-workspace-candidate-r5.log`，退出状态 0。
- 部署状态：`部署未验证`。本次未运行真实 integrator loop，也未读取或改变目标运行环境的部署；目标运行环境是否未修复或已经部署，本次均未核实。
- 已知缺口：目标运行环境的空值来源仍未查明。公开议题 [#73](https://github.com/thinkseek/spawn/issues/73) 还包括非空 verdict 的 stale 重试与升级，本项只关闭 empty/whitespace 参数导致的同型循环。
- 下一步：在隔离的 integrator 环境确认连续参数 abort 的计数与 halt，再对 #73 的不可变 stale verdict 单独设计 park/alert。

## 维护协议

1. **派活先读。** 派活 Agent 在修改 FM-01 至 FM-19 涉及的能力前，先读本图和对应代码，并在任务说明中写明 Feature Map ID。图中状态只对应其基线提交。
2. **完成回写。** 同一变更更新能力、入口、行号、实现状态、测试证据、部署状态、缺口和下一步。测试证据写实际命令、环境和结果；只有源码与回归测试才能关闭代码缺陷。
3. **部署另记。** 使用 `部署已验证` 时，在该字段写明 `环境：`、`commit：`、`命令：`、`结果：`、`日期：`。只完成代码合入时保持 `部署未验证`。目标运行环境或其他主机的运行观察先写入该项目运行记录，不能单独把代码状态改成已修复。
4. **固定复核。** 固定维护 Agent 为 `feature-map-maintainer`。它在核心链路变化、FM-18/FM-19 状态变化、部署后或收到复核请求时检查漂移。这是固定维护职责名，不表示仓库内存在常驻进程。
5. **事实分源。** 实现以代码和测试为准，运行状态以项目运行数据为准，后训练结论以后训练文档为准。公开仓库只保留可公开引用和通用环境说明，不写机器私有路径、凭据或私有数据。
6. **每次校验。** 在仓库根目录运行：

   ```bash
   python3 -I scripts/check-feature-map.py
   python3 -I scripts/check-feature-map.py --self-test
   git diff --check
   ```

   第一项检查每个 Feature Map 条目的字段、状态词、引用文件和行号。第二项用无效文件、越界行号、缺失字段、非法状态词、重复 ID、私有路径和缺失部署元数据做 7 个负例自检。维护 Agent 按需复核时也必须运行这三项。
