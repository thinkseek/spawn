# Spawn RSI 方案 v2

结论：RSI 需要四件事——控制器、可信判分器、基线落地、任务源与复合。v1 四件全做。

判分器的隔离边界是**进程/容器**，不是文件路径。把脚本复制到 `/opt` 不构成隔离：候选的测试代码以同一 Unix 身份运行，能覆写编译器包装、目标目录和控制器二进制。这是 v1 与 v2 之间最大的修正。

## 1. 从源码验证的前提

设计被五条事实约束，逐条验证过。

| # | 事实 | 后果 |
| --- | --- | --- |
| 1 | `EventKind` 是封闭 serde 枚举，无 `#[serde(other)]`（`event.rs:126-256`）。未知 `name` 在 `record_from_row` 映射为 `StoreError::Corrupt`（`spawn-postgres/src/lib.rs:94-97`），`read_global` 对每行反序列化 | 向 `spawn_events` 写任何 `rsi.*` 事件都会让下一次 `spawn tick` 退出 1。**RSI 状态只能进表，不能进事件日志** |
| 2 | `spawn_append`、`spawn_submit`、`spawn_claim`、`spawn_finish` 都是 INVOKER；只有 `spawn_gateway_insert` 与 `spawn_read_*` 是 SECURITY DEFINER | 只授 execute 会让调用方 42501。受限角色必须改造成 definer + 显式调用方校验 |
| 3 | `spawn tick` 把本进程的 `SPAWN_DATABASE_URL` 注入终端（`main.rs:178-181`），终端和 tick 同权 | 必须新增 `SPAWN_TERMINAL_DATABASE_URL`，只把受限 URL 注入终端 |
| 4 | 视图的 public select 已撤销，读取走 `spawn_read_*` 授权过滤函数（`schema.sql:573-593`），`spawn_grants` 无种子行 | 控制器必须有 principal 与 grant 行 |
| 5 | `gate-on-digest.sh:8` 写死 `REPO_DIR`，`:70` 执行 `bash "$REPO_DIR/scripts/e2e-smoke.sh"`；`e2e-smoke.sh` 不接受位置参数，`root` 由 `$0` 推导 | 只改两行不够，`/opt` 副本必须改造成接受显式路径，否则每次门禁都在 `/opt` 里 `cargo build` 失败 |

## 2. Gate 0：开工前必须验证

四项。P1 决定落地机制；I1 决定判分器是否可信。

| # | 探针 | 成功标准 | 失败后果 |
| --- | --- | --- | --- |
| P1 | `layerfs --project P branch fork b2 <commit_id>` | 退出 0 且 b2 内容等于该 commit | 落地走分支 B（外部 git 基线） |
| P2 | `layerfs ... workspace exec <ws> --stream -- cat .spawn/candidate.patch` | stdout 返回补丁内容 | 候选产物离不开沙箱，方案停止 |
| P3 | `echo x \| layerfs ... workspace exec <ws> --stream -- sh -c 'cat > /tmp/x'` | 内容为 x | 分支 B 不可用 |
| I1 | 容器内 `cargo test` 无法读写宿主 `$HOME`、kache、target | 写入被拒 | 判分器不可信，全部门禁结论无效 |

P2 的证据最强：`exec_prefix`（`layerfs.rs:41-52`）已用同一通道承载 tmux。P1 证据最弱：`branch fork <name>` 不带 source 出现在 `layerfs.rs:126-141`，source 槽位只由注释暗示。

## 3. 架构

新增一次性进程 `spawn evolve`，与 `spawn tick` 并列由 cron 驱动。

```text
*/1 * * * * spawn tick   --project spawn-rsi
*/1 * * * * spawn evolve --project spawn-rsi
```

`spawn evolve` 每次：取 `pg_try_advisory_lock`（单飞）→ 从 `rsi_*` 表与视图重建状态 → 推进一步 → 退出。不折叠事件日志推导阶段，因为事件不可靠且封闭。

职责：

| 组件 | 职责 |
| --- | --- |
| `spawn tick` | 不变 |
| `spawn evolve` | 选任务、开迭代、读候选、跑门禁、落地、停 |
| `scripts/gate.sh` | 在 digest 容器内跑门禁，输出一行 JSON |
| digest `/opt/spawn-gate/` | 可信门禁副本 + **容器镜像**，只由人更新 |

## 4. 状态机

阶段存 `rsi_iterations.phase`，由表和视图计算转移条件。不折叠事件。

一次迭代 = 一个 agent、一个 session、一个 execution、一个分支、一个工作区。编号绑定 `agent_rsi_<n>`：`BranchName::for_agent` 让分支等于 agent id（`id.rs:189-193`），共用固定 agent 会把被拒提交留在同一分支上。

| 阶段 | 转移条件 | 动作 |
| --- | --- | --- |
| `Idle` | 无 phase ∉ {Landed, Rejected, Aborted} 的迭代 | 见 4.1 |
| `Starting` | 有 Start 命令 | 见 4.2 |
| `Working` | execution 活跃，且未见完成标记 | 无动作 |
| `Proposing` | 完成标记存在 | 读候选 → 写 `rsi_iterations.patch` → `submit(Stop)` |
| `Gating` | 候选已记录 | 跑门禁 |
| `Landed` / `Rejected` / `Aborted` | 终态 | 无 |

**4.1 Idle 必须原子。** 写 `rsi_iterations` 与 `spawn_submit(Start)` 在**同一个 SQL 事务**里完成。分两步做，中间被杀会留下无命令的悬空迭代，任何阶段都不匹配。幂等键 `rsi-<n>-start`。

**4.2 Starting 的三个出口。** `agent_executions` 出现 → `Working`；`agent_tasks.succeeded=false` → 重试一次后 `Rejected`；`type_prompt` 超时也要走这条——它返回 `Failed` 但不写 `execution.ended`，`execution.started` 已写（`bridge.rs:481-493`），所以"有 execution"和"命令失败"会同时为真。以 `agent_tasks.state='finished'` 为准优先判定。

**4.3 完成标记，不是 `turn.stopped`。** `turn.stopped` 只表示本轮模型停止。任务提示词要求 agent 结束时写 `.spawn/done`，控制器只在存在该文件时进入 `Proposing`；否则提交 `Input` 继续。否则多回合任务在第一回合就被 Stop。

**4.4 控制器自己抓屏。** 不读 `screen.observed` 的历史值——它只在 Bridge 打字时写入，等待期间不刷新。新增一个 host 侧读命令（`tmux capture-pane` 或 `workspace exec`）在决策前采样。否则"最新读数为 ready"会变成不可离开的死锁。

## 5. 任务源与复合

v1 必须产生复合，否则 20 轮后无法证明任何事。

**任务来源**：人写的 `rsi/backlog/NNN-<slug>.json`，**不在 agent 可改范围内**。

```json
{
  "id": 7,
  "title": "Input 在非 Ready 时重试",
  "prompt": "...",
  "allowed_paths": ["crates/spawn-core/src/bridge.rs"],
  "accept": "cargo test -p spawn-core --test api input_waits_for_ready -- --exact",
  "accept_must_fail_on_baseline": true,
  "max_tool_calls": 60,
  "max_wall_minutes": 45,
  "meta": false
}
```

**复合机制（三件，缺一则退化为 PR 机器人）**：

1. **`accept` 必须在基线上为红、在候选上为绿。** `cargo test <filter>` 在零匹配时退出 0，所以过滤器必须用 `-- --exact` 并先断言基线失败。这同时封掉"空改动也 PASS"。
2. **失败回灌。** 被拒后，把该次的 `diff`、门禁日志尾部、失败分类写入 `rsi_attempts`，下一次尝试的 prompt = 任务 prompt + 全部历史尝试摘要。`(task_id, attempt)` 为键。
3. **`candidate_commit != baseline_commit`**，且非空 diff，否则直接 `Rejected`。

**新增表**：

```sql
create table rsi_attempts (
    task_id     integer not null,
    attempt     integer not null,
    iteration   integer not null,
    diff        text,
    gate_log    text,
    classifier  text,
    accept_ok   boolean,
    created_at_ms bigint not null,
    primary key (task_id, attempt)
);
```

## 6. 判分器

**6.1 容器隔离（v2 的核心修正）**

候选的构建与测试在一次性容器内运行：

| 项 | 设置 |
| --- | --- |
| 镜像 | 预装 rust 1.95、git、cargo；无控制器凭据 |
| 可信脚本 | `/opt/spawn-gate` 只读挂载 |
| 候选树 | 复制进容器，不 bind mount 可被并发修改的宿主路径 |
| `CARGO_TARGET_DIR` | 容器内，**不共享**宿主 kache |
| 宿主 `$HOME` | 不挂载 |
| 数据库 | 容器内起 postgres:16-alpine，端口动态分配 |
| 网络 | 仅代理；不挂 host network |
| 判决 | 只经 stdout 一行 JSON 交给控制器 |

代价：kache 命中率下降，门禁从估算 2–4 分钟升到 6–12 分钟。这是隔离的价格。若后续证明容器启动开销不可接受，再评估只读挂载 kache 缓存（需 kache 支持只读）。

**6.2 门禁对象必须由补丁确定**

判据校验的是 `candidate.patch`（agent 自写），门禁跑的是抽取的整棵树。两者必须有约束：

```text
gated_tree := clean checkout(baseline_commit) + git apply --index candidate.patch
拒绝条件：   git diff baseline_commit -- gated_tree 与 candidate.patch 不一致
```

绝不门禁原始工作区树。同时拒绝候选树中存在基线没有的 `.cargo/`、`Cargo.toml` 覆盖、`rust-toolchain.toml`，除非在 `allowed_paths` 内。

**6.3 判据**

| # | 判据 | 失败处理 |
| --- | --- | --- |
| 1 | `git apply --check` 通过，且树 ≡ 基线 + 补丁 | 拒绝，不跑门禁 |
| 2 | 改动路径 ⊆ `allowed_paths`，≤ 400 行 / 64 KiB / 20 文件 | 拒绝，不跑门禁 |
| 3 | 非空 diff，`candidate_commit != baseline_commit` | 拒绝 |
| 4 | `accept` 在基线上失败、在候选上通过 | 拒绝 |
| 5 | 容器门禁退出 0，stdout JSON 的 `verdict` 为 `pass` | 拒绝 |

**6.4 失败分类**

- 门禁容器内的基础设施失败（镜像拉取、代理、端口）→ `infra`，独立重试 3 次，中止本次迭代但不计 `Rejected`。
- **基础设施失败累计 3 次也 HALT**，否则候选制造 infra 失败就能绕过连续拒绝的停止条件。
- 分类依据是门禁脚本输出的分阶段 JSON，不是"失败发生在 `== fmt ==` 之前"——crate 与工具链解析发生在 fmt 之后，网络故障会被误判成候选失败。

## 7. 落地

基线是**一个 commit id**，落地是**一次写入**。

### 分支 A（P1 成功）

```text
开迭代:  layerfs branch fork agent_rsi_n <baseline_commit>
结束:    candidate_commit = 门禁跑过的那棵树的 commit
PASS:    baseline_commit := candidate_commit
```

预建分支后 Bridge 的 `ensure_branch` 收到 `NameTaken`/`Exists`，映射为 `Ok`（`layerfs.rs:135-137`），成为空操作。候选从基线 fork，落地永远 fast-forward，门禁结论对落地树一字不差。

**落地需要把门禁树写回 workspace**（`layerfs workspace commit` 只接受已挂载的 workspace），所以分支 A 也依赖 P3。P1 成功后仍需确认 P3。

### 分支 B（P1 失败，P3 成功）

基线是 digest 上的普通 git 仓库。LayerFS 只作可丢弃编辑沙箱。

```text
开迭代:  mount 后 echo <tarball> | layerfs workspace exec <ws> -- sh -c 'tar -xf -'
PASS:    CAS(expected_head) → git apply → commit → tag rsi-landed-<n>
回滚:    git reset --hard <previous_landed>
```

CAS 是必要的：控制器单飞，但门禁子进程可能比锁持有者活得久。

**被拒迭代的分支必须 GC。** 适配层没有删分支的能力，按 1.5 GB/commit 估算，20 次拒绝约 30 GB。必须定义清理步骤，或禁止在沙箱内 build。

## 8. 记忆

新增三张表，随 `schema.sql` 由 `init-db` 安装：`rsi_iterations`、`rsi_gate_runs`、`rsi_attempts`。

一次迭代的真值不是三元组。判决还依赖门禁镜像摘要、工具链版本、`accept` 命令文本与测试过滤器的匹配数。全部记入 `rsi_gate_runs`：

```sql
create table rsi_gate_runs (
    iteration    integer not null,
    run_no       integer not null,
    gate_image   text not null,
    gate_version text not null,
    verdict      text not null,
    classifier   text not null,
    passed_tests integer,
    e2e_total_ms bigint,
    log          text,
    primary key (iteration, run_no)
);
```

`e2e_total_ms` 需要先实现——当前 e2e 输出分阶段耗时，但没有统一的聚合字段。没有它就不要写这条护栏。

## 9. 安全

**9.1 终端与控制器分权**

| 角色 | 持有 |
| --- | --- |
| `spawn_controller` | `spawn_submit`/`spawn_claim`/`spawn_finish`；`rsi_*` 读写；`spawn_grants` 中 `*` 授权 |
| `spawn_terminal` | 终端环境注入的 `SPAWN_TERMINAL_DATABASE_URL` |

`spawn tick` 用控制器角色运行，只把 `SPAWN_TERMINAL_DATABASE_URL` 注入 `BridgeConfig.environment`。这是新增的环境变量，代码改动在 `main.rs:178-181`。

**9.2 函数必须做调用方校验**

`spawn_append` 等是 INVOKER，给表权限就等于给全部能力。两条路选一：

- 改成 `SECURITY DEFINER` + 固定 `search_path`，内部按 `session_user` 校验 `(source, event_name)` 白名单。`spawn_terminal` 只能写 hook 来源的事件，不能写 `observation.recorded`、`execution.ended`、`artifact.committed`。
- 或给表权限，同时用行级安全策略限制可写行。

只授 execute 的做法不可行，会出现 42501 且被 `main.rs:263-272` 静默吞掉。

**9.3 控制器的读模型不可由 agent 伪造**

阶段从表和视图计算，但 `agent_status`、`agent_executions` 是事件投影。若 `spawn_terminal` 能写 `observation.recorded`，agent 就能伪造 `WAITING` 让控制器提交 Input。9.2 的白名单是这条的防线，不是可选项。

**9.4 停止条件**

| 条件 | 阈值 | 动作 |
| --- | --- | --- |
| 单迭代门禁失败 | 3 | Rejected |
| 连续 Rejected | 3 | HALT |
| 基础设施失败 | 累计 3 | HALT |
| 单迭代墙钟 | 45 分钟 | Aborted |
| 单迭代工具调用 | 60 | Aborted |
| 门禁墙钟 | 20 分钟 | Aborted |
| 丢失执行 | 2 | Rejected |
| 累计迭代 | 可配置；阶段 4 与阶段 5 共用同一计数，需人工续期 | Idle 停 |

HALT 由一次性事件计数触发，清除需人工删除 `rsi/HALT` 并把连续计数归零。

**9.5 落地后的残留风险**

候选落地后，下一轮的 `spawn tick` 与 `spawn evolve` 由落地代码构建。一个通过门禁但恶意的改动会获得控制器权限。门禁只证明编译、测试、冒烟通过，不证明无恶意。缓解只能是人审抽样，方案不解决这一层。

## 10. Meta

**可改**：`scripts/rsi-*.sh`、`docs/rsi.md`。**不可改**：`/opt/spawn-gate/`（脚本与镜像）、`rsi/backlog/`。

**落地三条全需**：

1. 普通门禁通过。
2. `rsi-selftest.sh` 通过——**它必须在 `/opt/spawn-gate/` 内，不在 `scripts/rsi-*.sh` 集合里**。否则一个补丁可以同时改状态机和它的验收。
3. 人类在**基线仓库**里写 `rsi/approvals/<task-id>-<sha256(task+patch)>.ack`。键控哈希而非 task id，否则批准一次后可无限改写同一任务。

**执行隔离**：cron 运行 `$SPAWN_RSI_HOME/bin/spawn`，由人从构建产物提升，上一版保留为 `.prev`。

v1 默认关闭 meta，先用 20 轮验证产品代码路径能收敛。

## 11. 成本与人日

估算，非承诺。

| 项 | 值 |
| --- | --- |
| 门禁单次 | 6–12 分钟（容器冷启动 + 无 kache） |
| 单迭代墙钟 | 30–90 分钟 |
| 单迭代 token | 15 万–50 万输入，3 万–8 万输出（估算） |
| 单迭代磁盘 | 源码树约 2 MB；禁止沙箱内 build，否则单 commit 可达 1.5 GB |

| 项 | 人日 |
| --- | --- |
| Gate 0 探针 + 隔离验证 | 2 |
| 数据库角色、函数改造、`SPAWN_TERMINAL_DATABASE_URL` | 3 |
| `spawn evolve` 状态机 + 原子 Idle | 4 |
| 门禁容器与 `gate.sh` | 3 |
| 补丁通道、树校验、`accept` 红绿 | 2 |
| 落地 CAS + GC | 2 |
| 复合：attempts 表与 prompt 回灌 | 2 |
| backlog、提示词、selftest | 2 |
| 真实端到端 | 2 |
| **合计（分支 A）** | **22** |

分支 B 额外 +2。

## 12. 阶段

**最便宜的否证放在最前。** 阶段 1 就要用真实 agent 跑通一次"产出通过门禁的补丁"，而不是推到第 9 天之后。

| 阶段 | 内容 | 工期 | 验收 |
| --- | --- | --- | --- |
| 0 | P1/P2/P3 + I1 探针 | 2 天 | 四项有确定结论；失败则停止 |
| 1 | 绝对最小闭环：单个硬编码任务，真实 agent，补丁经门禁在容器内通过 | 4 天 | 一次真实迭代落地，基线前移。**这是整个方案的否证点** |
| 2 | 角色分离 + 函数白名单 + 终端 URL | 3 天 | 终端角色伪造 `observation.recorded` 被拒 |
| 3 | `spawn evolve` 状态机 + 原子 Idle + 完成标记 + 自抓屏 | 5 天 | 假 engine 连续 3 次迭代，含 1 PASS 1 Rejected |
| 4 | 复合：attempts 回灌 + accept 红绿 + 非空 diff | 3 天 | 同一任务第二次尝试的 prompt 含第一次的 diff 与门禁日志 |
| 5 | 连续 20 轮 | 3 天 | 无连续 3 次 Rejected；测试数不降；不为空产出 |
| 6 | meta 双钥匙 | 2 天 | 一条 meta 变更走完三条 |

阶段 4 与阶段 5 共用迭代计数，阶段 5 前需人工抬高上限。

## 13. 不做什么

- **不写 `rsi.*` 事件。** 封闭枚举，写进去产品链路就停。
- **不把判分器隔离当文件问题。** 边界是容器。
- **不做 daemon。**
- **不做并行种群。** 门禁是 pass/fail，选不出更好的那个。
- **不让 agent 改 backlog 或判分器。**
- **不在沙箱内 build。** 无网络无 registry，失败快，且 commit 体积失控。
- **不假设 Claude 权限标志。** `claude_arguments`（`bridge.rs:717`）不设权限模式，先用 `claude --help` 确认。包装脚本必须在容器可见的固定路径。
- **不把屏尾 Error 当可选修复。** `screen.rs:224` 在 Ready 之前检查 body Error，Rust 编译错误会让 `Input` 永久被拒（`bridge.rs:635-640`）。`--resume` 重放含错误的历史，Stop+Resume 清不掉。
