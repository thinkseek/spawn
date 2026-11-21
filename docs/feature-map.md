# Spawn Feature Map

## 读法与证据边界

代码基线为提交 `fec2e674fee5cf033d7ee8feaf4b2f363261843d`。行号只对这一提交有效。后续修改必须同步更新引用。

- 实现状态只使用 `已实现`、`部分实现`、`未实现`、`已知缺陷`。
- 部署状态只使用 `部署未验证`、`部署已验证`、`未部署`、`不适用`。本次没有执行部署，也没有读取 n252 的进程、数据库或日志，因此没有把源码存在写成线上已部署。
- 测试证据给出测试源码位置。它不等于本次测试通过。每次回写必须另外记录实际运行的命令、环境和结果。
- 项目运行数据以 PostgreSQL 事件、主机归档和运行记录各自的事实来源为准。后训练文档以其所属项目的事实来源为准。本图只连接这些边界，不复制其数据，不把机器私有路径写进公开代码仓。
- FM-18 和 FM-19 是当前源码中的未修缺陷。n252 的运行状态不能证明代码已修复；代码修复也不能证明某个环境已经部署。

## 核心编排与 PostgreSQL

### FM-01 Mission 与 task 控制流

- 能力：创建和配置 mission，记录 owner push、hold/release，驱动 task 状态及 gate verdict 折叠。
- 入口：`spawn mission ...`、`spawn task ...`、SQL control-record 写入与 refold。
- 代码：`crates/spawn-cli/src/main.rs:120-122`、`crates/spawn-cli/src/missions.rs:458-594`、`crates/spawn-core/src/mission.rs:14`、`crates/spawn-postgres/sql/schema.sql:1710`、`crates/spawn-postgres/sql/schema.sql:1768`。
- 实现状态：`已实现`。配置和任务变化先成为记录，SQL 再折叠为读模型；`aborted` 本身不把 task 推进到完成态。
- 测试证据：`crates/spawn-core/src/mission.rs:87-125` 固定状态值和未知值错误；`crates/spawn-postgres/tests/control.rs:51-135` 覆盖 mission/resident 控制记录折叠。本次在 Darwin/aarch64 运行 `cargo test -p spawn-core mission::tests`，2 个测试通过。
- 部署状态：`部署未验证`。需要同一套 CLI、schema 和 loop；本次未执行端到端 mission 生命周期。
- 已知缺口：accepted 后的 landing/landed 依赖 integrator；仅有控制命令成功不能证明工作已经落地。
- 下一步：在一次性数据库中覆盖 assigned→reported→accepted→landing→landed，并分别保留 aborted、stale、conflict 的证据。

### FM-02 Objective 调度、dispatcher 与 stall 升级

- 能力：从 backlog 分配 objective，处理 attempt/yield、worker slots、continuation，重复生成由记录决定的投递计划，并升级停滞。
- 入口：`spawn mission objectives ...`、`spawn integrate` 分配 objective；`spawn loop` 运行 dispatcher 和 stall；核心函数为 `objectives::assign_next`、`dispatch::decide_with`。
- 代码：`crates/spawn-cli/src/objectives.rs:974-1060`、`crates/spawn-cli/src/integrate.rs:2550-2565`、`crates/spawn-cli/src/dispatch.rs:593-645`、`crates/spawn-cli/src/commands/loops.rs:406-447`、`crates/spawn-cli/src/dispatch/stall.rs:1-50`。
- 实现状态：`已实现`。dispatcher 由记录和时钟生成计划；stall 生成 supervisor 通知，不直接修复阻塞。
- 测试证据：`crates/spawn-cli/src/objectives.rs:1572-1634` 覆盖 slot 与 yield；`crates/spawn-cli/src/dispatch.rs:1299` 覆盖同事实重复决策；`crates/spawn-cli/src/dispatch/stall/tests.rs:133-174` 覆盖停滞计时起点。
- 部署状态：`部署未验证`。backlog 和 scenarios 是 controller 输入；本次未核对部署副本或真实 worker。
- 已知缺口：backlog JSON 与 scenario 文件不是 PostgreSQL 内的任务事实；部署时需要额外保证它们与目标 revision 一致。
- 下一步：运行 scheduler、deterministic dispatch 和 stall 测试，再核对 deployed controller 的 backlog、scenarios 与目标 revision。

### FM-03 PostgreSQL 追加事件日志

- 能力：脱敏后追加事件，按 source key 幂等，维护稠密 session 序号、全局顺序、单 open execution 和可重建 projection。
- 入口：`EventLog::{append, read_global, open_execution}`、SQL `spawn_append`、`spawn_append_trusted`。
- 代码：`crates/spawn-core/src/port.rs:58-74`、`crates/spawn-postgres/src/event_log.rs:58-110`、`crates/spawn-postgres/sql/schema.sql:229-366`。
- 实现状态：`已实现`。写入持事务级锁；重复 source key 返回原行；事件与 session projection 在同一 SQL 调用中更新。
- 测试证据：`crates/spawn-core/src/contract.rs:42-117` 是共享一致性契约；`crates/spawn-postgres/tests/api.rs:72`、`crates/spawn-postgres/tests/api.rs:80`、`crates/spawn-postgres/tests/api.rs:95` 覆盖 PostgreSQL 契约、幂等与序号；`crates/spawn-postgres/tests/api.rs:1126-1182` 覆盖 projection 重建。
- 部署状态：`部署未验证`。本次未连接数据库，也未测量并发写入或 projection 漂移修复。
- 已知缺口：顺序和 projection 保证要求写方经过 SQL 函数；直接写表不在契约内。
- 下一步：在一次性数据库中运行共享契约、并发 append 和强制 stale projection 测试。

### FM-04 幂等命令队列与事件唤醒

- 能力：幂等 submit、单 session 串行 lease、waiting-input 让行、release/finish、watermark、LISTEN/NOTIFY 和定时回退。
- 入口：`CommandQueue::{submit, claim, release, finish}`、`PostgresStore::{watermark, listen, next_lease_ms}`。
- 代码：`crates/spawn-postgres/src/command_queue.rs:21-67`、`crates/spawn-postgres/sql/schema.sql:368-572`、`crates/spawn-postgres/src/wake.rs:34-86`、`crates/spawn-cli/src/commands/loops.rs:499-555`。
- 实现状态：`已实现`。submit 与 `command.accepted` 同事务；finish 与 `command.finished` 同事务；丢失通知由 watermark 和到期时间兜底。
- 测试证据：`crates/spawn-postgres/tests/api.rs:117-267` 覆盖幂等与串行竞争；`crates/spawn-postgres/tests/waiting.rs:67-129` 覆盖 waiting 与 lease；`crates/spawn-postgres/src/wake.rs:134-228` 覆盖通知和到期调度。
- 部署状态：`部署未验证`。本次未运行 submit→claim→finish，也未断开 LISTEN 连接。
- 已知缺口：通知不代表每个事件都会触发所有重步骤；screen/transcript 仍按轮询读取。
- 下一步：运行 queue 和 wake 测试，在 staging 断开 listener，确认定时 watermark 继续且不形成紧密循环。

### FM-05 Schema 迁移、受限角色与恢复事实

- 能力：显式 `init-db` 原地迁移，回填 projection，提供 per-database 受限登录和持久化 recovery facts。
- 入口：`spawn init-db`、`PostgresStore::migrate`、`role_url`、`spawn_host_record`。
- 代码：`crates/spawn-cli/src/main.rs:100-103`、`crates/spawn-postgres/src/lib.rs:265-329`、`crates/spawn-postgres/sql/schema.sql:106-177`、`crates/spawn-postgres/src/gateway.rs:89-169`、`crates/spawn-postgres/src/lib.rs:467-501`、`crates/spawn-postgres/src/recovery.rs:27-106`、`crates/spawn-postgres/sql/schema.sql:3591-3631`。
- 实现状态：`已实现`。普通 `open_store` 只连接；迁移是独立部署步骤。受限登录由 owner 密码派生，recovery writer 只接受 owner。
- 测试证据：`crates/spawn-postgres/tests/upgrade.rs:81` 覆盖旧表升级；`crates/spawn-postgres/tests/api.rs:2888-2938` 覆盖迁移竞争；`crates/spawn-postgres/tests/roles.rs:108-149` 覆盖数据库间角色隔离；`crates/spawn-postgres/tests/recovery.rs:56`、`crates/spawn-postgres/tests/recovery.rs:117` 覆盖 recovery writer。
- 部署状态：`部署未验证`。本次未执行 init-db、角色连接或数据库重启。
- 已知缺口：每次 schema application 都会对整个事件日志做分组回填；大日志版本化优化尚未实现。组合 recovery 读 API 缺少直接命名的集成测试。
- 下一步：在代表性副本上测量迁移并增加版本门控；补 recovery_sessions/signals/facts 的一次性数据库测试。

## Resident、worktree 与 Claude Code

### FM-06 Resident 审批、退休、保活与恢复

- 能力：propose/approve/retire resident，维持已批准 worker，按持久化恢复事实选择 resume、wait、handoff 或 final。
- 入口：`spawn resident propose|approve|retire`、`supervise::keep_residents`、loop recovery step。
- 代码：`crates/spawn-cli/src/missions.rs:762-777`、`crates/spawn-cli/src/supervise.rs:348-427`、`crates/spawn-cli/src/commands/loops.rs:390-447`、`crates/spawn-cli/src/recover.rs:873-952`。
- 实现状态：`已实现`。retire 会进入 open-task 取消路径；resident 后续投递还受 FM-18 的通知顺序约束。
- 测试证据：`crates/spawn-postgres/tests/control.rs:51-80` 覆盖 propose/approve/retire；`crates/spawn-cli/src/supervise.rs:595-637` 覆盖 revival backoff 与 handoff；`crates/spawn-cli/src/recover.rs:2400`、`crates/spawn-cli/src/recover.rs:2439` 覆盖恢复退避和 handoff/final。
- 部署状态：`部署未验证`。本次未进行 owner 审批、退休、进程重启或恢复演练。
- 已知缺口：新的 native session 不自动继承旧对话上下文；恢复依赖 workspace、transcript、report 和记录。
- 下一步：在隔离环境覆盖 propose→approve→retire、resume/handoff，并与 cancel-notice 场景一起验证。

### FM-07 分支导出、worktree、checkpoint 与 set-aside

- 能力：从 agent 分支导出无 `.git` 的工作目录，做 checkpoint CAS、main merge、冲突续作和取消工作的 set-aside。
- 入口：`GitWorkspace`/`WorkspaceStore` 的 branch、mount、checkpoint、merge、set-aside。
- 代码：`crates/spawn-host/src/git.rs:54-116`、`crates/spawn-host/src/git.rs:1163-1321`、`crates/spawn-cli/src/set_aside.rs:77-154`、`crates/spawn-cli/src/main_merge.rs:1`。
- 实现状态：`已实现`。这里的“worktree”是 `git archive` 式 plain export，不是带 `.git` 的常规 `git worktree`；分支头保存快照。
- 测试证据：`crates/spawn-host/tests/api.rs:109-170` 覆盖真实 git branch/mount/checkpoint；`crates/spawn-host/src/git.rs:3030-3122` 覆盖大文件边界；`crates/spawn-cli/src/set_aside.rs:190-205` 覆盖取消通知内容。
- 部署状态：`部署未验证`。本次未创建真实 agent workspace 或合并冲突。
- 已知缺口：默认单文件 100 MiB checkpoint 上限不适合大数据集和模型产物；超限文件需要独立 artifact 存储。
- 下一步：运行 real-git、main-merge、set-aside 测试，并为大型数据产物定义独立保留与血缘方案。

### FM-08 tmux 与 Claude Code 执行生命周期

- 能力：建立精确 tmux session，启动 Claude Code runner，分段输入长 prompt，处理 trust dialog、exit、lost runner 和终止清理。
- 入口：`spawn run -- claude ...`、`Bridge::type_prompt`、`TmuxHost::{start, stage_text, runner, terminate}`。
- 代码：`crates/spawn-core/src/bridge.rs:3459-3559`、`crates/spawn-host/src/tmux.rs:608-761`、`crates/spawn-cli/src/commands/engine.rs:24-45`、`crates/spawn-host/src/tmux.rs:831-943`。
- 实现状态：`已实现`。runner 写退出状态；Bridge 结合 transcript、pane 和 runner 事实判定 exited/lost/stopped。
- 测试证据：`crates/spawn-host/tests/api.rs:514-546` 覆盖长文本；`crates/spawn-host/tests/api.rs:548-624` 覆盖环境不入 argv；`crates/spawn-host/tests/runner.rs:174` 覆盖 runner 身份；`crates/spawn-host/tests/runner.rs:410`、`crates/spawn-host/tests/runner.rs:496` 覆盖终止等待；`crates/spawn-core/tests/waiting_inputs.rs:294` 覆盖 ready 后输入。
- 部署状态：`部署未验证`。本次未启动 tmux 或 Claude Code。
- 已知缺口：权限绕过配置要求外层 sandbox；`setsid` 离开记录 session 的后代不在现有等待/SIGKILL 集合内。非 Linux 也没有等价 `/proc` 清理保证。
- 下一步：在目标 Linux 用隔离 tmux socket 做启动、长 prompt、外部 kill 和脱离 session 的故障演练。

### FM-09 Transcript、screen、归档与 Observer

- 能力：增量归一化 Claude Code JSONL 与 pane 屏尾，保存 append-only transcript copies，规则优先并在歧义时调用 Jev。
- 入口：`claude_line`、`read_claude_screen`、`TranscriptArchive`、`Observer::tick`、`JevJudge::judge`。
- 代码：`crates/spawn-core/src/ingest.rs:892-974`、`crates/spawn-core/src/screen.rs:355-409`、`crates/spawn-host/src/transcripts.rs:55-117`、`crates/spawn-core/src/observe/observer.rs:294-370`、`crates/spawn-jev/src/lib.rs:198-221`、`crates/spawn-host/src/watch.rs:161-175`、`crates/spawn-host/src/watch.rs:297-307`。
- 实现状态：`部分实现`。Linux 路径已具备这些能力。非 Linux 的 `unwatch` 仍调用仅 Linux 定义的 `release`，当前 Darwin 构建因此失败。
- 测试证据：`crates/spawn-core/src/ingest.rs:1083-1171` 覆盖事件归一与未知行；`crates/spawn-core/src/transcripts.rs:202-278` 覆盖归档和恢复；`crates/spawn-core/tests/observe_tiers.rs:180-338` 覆盖规则/Jev 分层。本次在 Darwin/aarch64 运行 `cargo test -p spawn-cli deliver::tests::a_stopped_resident_is_told_before_its_waiting_task_which_waits_for_the_notice -- --exact`，构建在 `crates/spawn-host/src/watch.rs:307` 报 E0599，依赖 spawn-host 的测试未执行。
- 部署状态：`部署未验证`。本次未读取真实 transcript、归档或 Jev relay。
- 已知缺口：解析依赖 Claude Code 版本格式。归档在 turn end 和 execution end 触发，不是连续复制。本地归档不提供跨主机容灾，非 Linux 还有上述编译缺口。
- 下一步：为 `unwatch` 增加非 Linux 分支和跨平台编译检查。引擎升级时新增真实 fixtures，并在目标环境演练 rewrite/delete/resume 与 loopback Jev 故障。

## Gate、integrator 与 fleet

### FM-10 黑盒 gate 与 sandbox

- 能力：用 controller scenarios 判定 base/candidate，保护路径，复用 base cache，在 bubblewrap、离线 Cargo 和受限网络中运行候选代码。
- 入口：`scripts/rsi/gate.sh <repo> <base> <candidate> <scenarios> [scenario]`、`gate::judge`。
- 代码：`scripts/rsi/gate.sh:5-52`、`scripts/rsi/gate.sh:128-149`、`crates/spawn-cli/src/gate.rs:1036-1167`。
- 实现状态：`已实现`。可信 verdict 来自私有文件且要求 gate 成功退出；host fault、no-room、cannot-judge 与 candidate rejection 分开。
- 测试证据：`scripts/rsi/test-gate.sh:157-206` 覆盖基本判定和 revision；`scripts/rsi/test-gate.sh:1318-1378` 覆盖 base 顺序与缓存；`scripts/rsi/test-gate.sh:1652-1683` 覆盖 host fault/base disagreement。
- 部署状态：`部署未验证`。当前 macOS worktree 不能证明 Linux bubblewrap、namespace 或私有 PostgreSQL 可用。
- 已知缺口：base cache 仍有 same-UID 信任限制；空 revision 的入口校验缺口单列在 FM-19。
- 下一步：在目标 Linux 运行完整 gate 自测；需要抵御同 UID 恶意进程时，把 cache 与执行放进独立 UID 边界。

### FM-11 Integrator freshness、verdict 与 CAS landing

- 能力：核对 published head 与 report checkpoint，构造两父 landing commit，绑定 verdict revision，以 CAS 推送 main，并处理 owner push。
- 入口：`spawn integrate`、report freshness、`landing_candidate`、`land`、`integrate/owner.rs`。
- 代码：`crates/spawn-cli/src/integrate.rs:1369-1522`、`crates/spawn-cli/src/integrate.rs:3769-3834`、`crates/spawn-cli/src/integrate.rs:4591-4697`、`crates/spawn-cli/src/integrate/owner.rs:1-60`、`crates/spawn-cli/src/integrate.rs:888-902`。
- 实现状态：`已实现`。连续 abort 由 `halt_reason` 计数；push 与记录之间仍需要恢复路径，进程内 judging 完成不等于 landed。
- 测试证据：`crates/spawn-cli/src/integrate.rs:5917-6006` 覆盖 freshness；`crates/spawn-cli/src/integrate.rs:7047-7149` 覆盖两父 landing；`crates/spawn-cli/src/integrate.rs:10878-10957` 覆盖 revision-bound verdict；`crates/spawn-cli/src/integrate.rs:12244` 覆盖 owner push 顺序。
- 部署状态：`部署未验证`。本次未执行 gate、landing 或 push。
- 已知缺口：真实结果依赖 remote、lease、base 与中断恢复；remote shadow verdict 当前不能决定 landing。
- 下一步：运行 freshness、CAS、中断补记、incompatible verdict 和 owner ordering 测试，再在 staged remote 验证并发 base 变化。

### FM-12 Fleet、alerts、serve 与人工输入

- 能力：SQL fleet/tail 读模型、alert fire/clear/notice、attention 计划、dashboard reader，以及受限 human input/key 写入。
- 入口：`spawn fleet|tail|alerts`、`spawn serve`、`/api/attention`、SQL gateway。
- 代码：`crates/spawn-postgres/src/fleet.rs:12-109`、`crates/spawn-cli/src/alerts.rs:448-501`、`crates/spawn-cli/src/serve/attention.rs:118-160`、`crates/spawn-postgres/src/gateway.rs:60-169`。
- 实现状态：`已实现`。reader 与 human-write 使用不同权限路径；attention 是瞬时 dispatcher plan，不提交命令。
- 测试证据：`crates/spawn-postgres/tests/dashboard.rs:73-179` 覆盖 board；`crates/spawn-postgres/tests/alerts.rs:60-128` 覆盖 alert 生命周期；`crates/spawn-cli/src/serve/attention.rs:619-704` 覆盖 attention parity；`crates/spawn-postgres/tests/human_input.rs:83-234` 覆盖人工输入与角色限制；`crates/spawn-postgres/tests/human_key.rs:117-187`、`crates/spawn-postgres/tests/human_key.rs:230-284` 覆盖 key 命令与权限。
- 部署状态：`部署未验证`。本次未启动 HTTP 服务、查询 live fleet 或验证 token/Origin/network ACL。
- 已知缺口：fleet board 不是进程健康证明；没有 backup record 本身不会触发 backup-age alert；采样和投影延迟会影响新鲜度。
- 下一步：运行 dashboard、alerts、attention 和 HTTP guard 测试，并用一次性数据验证 reader 不能写、human role 只能输入。

## 部署与数据连接

### FM-13 Gated deploy、不可变 controller 与 systemd

- 能力：验证 accepted verdict，构建和安装 artifact，迁移 schema，切换 controller，移动 main，重启 loop/integrate/serve/relay。
- 入口：`deploy/deploy.sh`、`deploy/controller.sh`、`deploy/cutover-controller.sh`、`deploy/systemd/*.service`。
- 代码：`deploy/deploy.sh:301-396`、`deploy/deploy.sh:400-504`、`deploy/controller.sh:146-207`、`deploy/systemd/spawn-loop.service:1-30`、`deploy/systemd/spawn-integrate.service:1-26`。
- 实现状态：`已实现`。部署脚本有恢复记录和 fake-live 测试；实际部署仍必须独立记录 commit、环境和 unit 健康。
- 测试证据：`deploy/test-deploy.sh:248-314` 覆盖 pending 恢复；`deploy/test-deploy.sh:369-467` 覆盖 drain/timeout；`deploy/test-controller.sh:70-181` 覆盖原子切换和保留。
- 部署状态：`部署未验证`。本次只读脚本，没有运行 deploy、systemctl 或健康检查。
- 已知缺口：写入新事件后的旧二进制回滚受版本兼容约束；cutover 含主机特定配置和无界 gate 等待。既有 engine 在 restart/resume 前保留旧环境。
- 下一步：在 GNU/Linux 隔离环境运行 fake-live deploy/controller 测试；真实部署后记录 installed commit、unit 状态、HTTP/DB 检查和新 engine 环境。

### FM-14 远程 gate runner 与 shadow judging

- 能力：打包两个 revision 和 controller，在隔离 runner 执行，校验 MANIFEST/verdict commits，收集结果，并做非权威 shadow 对比。
- 入口：`deploy/gate-runner/setup.sh`、`deploy/gate-runner.sh [--check]`、`SPAWN_GATE_SHADOW_RUNNER`。
- 代码：`deploy/gate-runner.sh:162-239`、`deploy/gate-runner.sh:650-807`、`deploy/gate-runner/remote.sh:329-435`、`crates/spawn-cli/src/integrate/shadow.rs:320-345`。
- 实现状态：`已实现`。远程结果只比较和记录；shadow 不决定 landing、halt、计数或预算。
- 测试证据：`deploy/test-gate-runner.sh:242-401` 覆盖成功、超时、传输重试和伪造 verdict；`deploy/test-gate-runner.sh:468-599` 覆盖资源与隔离；`crates/spawn-cli/src/integrate.rs:15276-15367` 覆盖 shadow 不做决定。
- 部署状态：`部署未验证`。本次未连接 SSH runner、私有数据库或远程 sandbox。
- 已知缺口：单个 in-flight shadow 且 offer 可跳过，不能形成完整 verdict 样本；fake runner 不证明真实 toolchain 兼容。
- 下一步：在 GNU/Linux runner 上执行自测和 `--check`；保持非权威，直到 revision-bound agreement 数据支持另行评审的授权设计。

### FM-15 例行清理与 maintenance timer

- 能力：按各自的名字、owner 和年龄清理 gate scratch、base cache 与私有 PostgreSQL templates；按精确文件名保留最新 N 份 deploy dump。
- 入口：`scripts/maintenance.sh`、`spawn-maintenance.service`、`.timer`、dry-run。
- 代码：`scripts/maintenance.sh:34-110`、`scripts/maintenance.sh:112-166`、`deploy/systemd/spawn-maintenance.service:5-24`、`deploy/systemd/spawn-maintenance.timer:4-13`。
- 实现状态：`已实现`。每一步独立报告失败；dry-run 只打印计划，不删除。
- 测试证据：`scripts/test-maintenance.sh:97-138` 覆盖 dry-run、保留、symlink、安全期限、幂等和独立失败。
- 部署状态：`部署未验证`。本次未执行清理或读取 timer 日志。
- 已知缺口：脚本和测试依赖 GNU find/touch/date；scratch age 必须大于 gate wall-clock，否则可能删除活跃现场。
- 下一步：在目标 GNU/Linux 先跑测试和 dry-run，逐项审查路径后再启用 timer 并检查首次真实运行。

### FM-16 加密备份与恢复演练

- 能力：加密 PostgreSQL dump、source/transcript bundle，执行上传预算检查，提供 R2 lock/lifecycle 保留配置，并在独立数据库验证恢复。
- 入口：`spawn backup dump|bundle|verify`、backup systemd timers；旧 shell 工具只作手工 legacy 路径。
- 代码：`crates/spawn-cli/src/backup.rs:853-935`、`crates/spawn-cli/src/backup.rs:1135-1224`、`crates/spawn-cli/src/backup.rs:2089-2222`、`deploy/systemd/spawn-backup-verify.service:5-24`。
- 实现状态：`已实现`。verify 对比事件数和 high-watermark，并清理独立 drill database；R2 lock/lifecycle 文件只是配置输入。
- 测试证据：`crates/spawn-cli/src/backup.rs:3020-3137` 覆盖 tiers、limits、bundle 和 suspect dump；`crates/spawn-postgres/tests/backup.rs:40-92` 覆盖记录权限；`deploy/backup/test-pg-restore-drill.sh:72-146` 只覆盖 legacy Docker drill。
- 部署状态：`部署未验证`。本次未访问 R2、密钥、真实备份或 drill server。
- 已知缺口：本地 stand-in 不证明 Cloudflare 权限或 object lock；verify 需要独立 CREATEDB 能力，密钥托管需要单独核验。
- 下一步：在一次性环境运行 backup tests 和真实恢复，独立读回 R2 规则，并记录 dump/bundle/verified 事实及恢复结果。

### FM-17 数据工程与后训练连接边界

- 能力：提供 SQL operational views、scorecard JSON、原始 transcript 归档、加密 bundle、objective METRIC，以及 Supply PA 依赖 store 和可选测试数据库输入。
- 入口：`spawn scorecard --json`、`spawn backup bundle`、`SUPPLY_PA_DEPS`、gate `METRIC`、`no-test-database`。
- 代码：`crates/spawn-postgres/sql/schema.sql:583-893`、`crates/spawn-cli/src/scorecard.rs:128-172`、`crates/spawn-host/src/transcripts.rs:55-117`、`crates/spawn-cli/src/backup.rs:1645-1971`、`crates/spawn-cli/src/runtime.rs:101-140`、`crates/spawn-cli/src/objectives.rs:171-216`、`crates/spawn-cli/src/gate.rs:96-132`。
- 实现状态：`部分实现`。运行数据采集、分析、备份和外部项目依赖输入已存在；未找到直接的 post-training、dataset export 或标注平台连接器。
- 测试证据：`crates/spawn-cli/src/scorecard/tests.rs:805-921` 覆盖 token 去重和 mission 归属；`crates/spawn-cli/src/runtime.rs:366-381` 覆盖依赖路径注入；`crates/spawn-host/src/transcripts.rs:154` 覆盖本地归档契约。
- 部署状态：`部署未验证`。本次未连接外部数据工程系统、Supply PA controller、在线 relay 或后训练平台。
- 已知缺口：SQL views 和 transcript 不是版本化训练集；原始内容可能含秘密。`SUPPLY_PA_DEPS` 只传依赖路径，在线 model relay 也不是训练管道。
- 下一步：如需要后训练连接，另定义版本化 export schema、脱敏、去重、血缘和删除策略；运行数据与后训练文档继续各守事实来源。

## 未修缺陷

### FM-18 未应答 cancel-notice 阻塞 resident 后续投递

- 能力：取消通知的顺序保证及其对同一 resident session 后续任务的影响。
- 入口：`CANCELLED_DELIVERIES` → `Work::blocked` → `next_up`/`deliver::orders`。
- 代码：`crates/spawn-postgres/src/missions.rs:69-95`、`crates/spawn-cli/src/deliver.rs:598-621`、`crates/spawn-cli/src/deliver.rs:1229-1241`。
- 实现状态：`已知缺陷`，未修复。`told=false` 会把整个 session 加入 blocked，过滤该 session 的所有 Assigned/Revise 后续任务。这里“未应答”指没有 `succeeded` 的 cancel-notice 命令，不是等待 resident 的文字 ack。
- 测试证据：`crates/spawn-cli/src/deliver.rs:3076-3133` 直接覆盖 notice open/failed 时阻塞、`told=true` 后 task 8 放行；`crates/spawn-postgres/tests/cancelled.rs:96-157` 覆盖 told 的 SQL 含义。本次在 Darwin/aarch64 运行 `cargo test -p spawn-cli deliver::tests::a_stopped_resident_is_told_before_its_waiting_task_which_waits_for_the_notice -- --exact`，因 FM-09 的 E0599 编译错误未执行测试体，不能标记为通过。
- 部署状态：`部署未验证`。未读取 n252 的运行数据；n252 是否运行不能改变当前源码判定。
- 已知缺口：阻塞粒度是整个 session，没有后续任务绕过未成功通知的路径。相关公开议题 [#76](https://github.com/thinkseek/spawn/issues/76) 记录同一 cancel-stop/notice 路径的另一恢复缺陷，不代替本项复现。
- 下一步：明确通知成功语义、阻塞范围和升级兜底，补 open/failed/restart 的端到端回归；代码修复与环境部署分别回写。

### FM-19 空 base/candidate 造成无进展 gate 空转

- 能力：normal gate mode 的 revision 输入校验，以及空 base 与空 candidate 在 integrator 中的不同分类。
- 入口：`gate.sh` 参数解析、`git rev-parse --verify`、`gate::judge_seeing_base_check` → `judge_in` → `decide`、`task.gated` fold。
- 代码：`scripts/rsi/gate.sh:355-357`、`scripts/rsi/gate.sh:668-676`、`crates/spawn-cli/src/gate.rs:1059-1083`、`crates/spawn-cli/src/gate.rs:1225-1300`、`crates/spawn-cli/src/integrate.rs:3092-3118`、`crates/spawn-cli/src/integrate.rs:3738-3740`、`crates/spawn-cli/src/integrate.rs:3575-3576`、`crates/spawn-cli/src/integrate.rs:888-902`、`crates/spawn-cli/src/integrate.rs:1551-1566`、`crates/spawn-cli/src/integrate.rs:4826-4845`、`crates/spawn-postgres/sql/schema.sql:1768-1772`。
- 实现状态：`已知缺陷`，未修复。standalone gate 对空或空白 base/candidate 都在 rev-parse 处 exit 3。空 candidate 在 base 未移动时记为 `aborted`，三次连续 abort 触发 halt。空 base 到达 integrator 后处理时，只要 source HEAD 可读且非空，`main_moved` 会把结果改写为 `stale`，不增加 abort 计数。两者都可能产生重复无进展尝试，机制不同。当前正常 task 的 base 从 `refs/heads/main` 解析，n252 的空值来源未查明；连续 integrate 至少 5 秒一轮，不是无限紧密循环。
- 测试证据：`scripts/rsi/test-gate.sh:194-205` 只有 nonexistent-ref 的 exit 3 用例；`crates/spawn-cli/src/integrate.rs:6296` 覆盖一般 abort。未找到 normal-mode empty/whitespace base/candidate 回归测试。本次只读 `git rev-parse --verify --quiet` 探测 empty、spaces、tab、newline，均返回 1 且 stdout 为空；未运行 full gate。
- 部署状态：`部署未验证`。未读取 n252 的运行数据；运行状态不能写成该输入校验已经修复。
- 已知缺口：standalone 与 integrator 都缺少 empty/whitespace 参数回归。空 candidate 会消耗 abort 计数，空 base 可走 stale 重试；现有证据不支持把两者统一写成三次 abort。相关公开议题 [#73](https://github.com/thinkseek/spawn/issues/73) 记录 stale 重试循环，不代替空参数复现。
- 下一步：在 gate trust boundary 拒绝 trim 后为空的 base/candidate，使用专用参数错误。分别补 standalone、空 candidate abort、空 base stale 的回归，并明确配置错误是否应计入 mission halt。

## 维护协议

1. **派活先读。** 派活 Agent 在修改 FM-01 至 FM-19 涉及的能力前，先读本图和对应代码，并在任务说明中写明 Feature Map ID。图中状态只对应其基线提交。
2. **完成回写。** 同一变更更新能力、入口、行号、实现状态、测试证据、部署状态、缺口和下一步。测试证据写实际命令、环境和结果；只有源码与回归测试才能关闭代码缺陷。
3. **部署另记。** 使用 `部署已验证` 时，在该字段写明 `环境：`、`commit：`、`命令：`、`结果：`、`日期：`。只完成代码合入时保持 `部署未验证`。n252 或其他主机的运行观察先写入该项目运行记录，不能单独把代码状态改成已修复。
4. **固定复核。** 固定维护 Agent 为 `feature-map-maintainer`。它在核心链路变化、FM-18/FM-19 状态变化、部署后或收到复核请求时检查漂移。这是固定维护职责名，不表示仓库内存在常驻进程。
5. **事实分源。** 实现以代码和测试为准，运行状态以项目运行数据为准，后训练结论以后训练文档为准。公开仓库只保留可公开引用和通用环境说明，不写机器私有路径、凭据或私有数据。
6. **每次校验。** 在仓库根目录运行：

   ```bash
   python3 -I scripts/check-feature-map.py
   python3 -I scripts/check-feature-map.py --self-test
   git diff --check
   ```

   第一项检查每个 Feature Map 条目的字段、状态词、引用文件和行号。第二项用无效文件、越界行号、缺失字段、非法状态词、重复 ID、私有路径和缺失部署元数据做 7 个负例自检。维护 Agent 按需复核时也必须运行这三项。
