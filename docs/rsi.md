# Spawn RSI 方案 v3

结论：RSI 是控制器 + 黑盒验收 + 基线前移 + 反馈回灌。一次迭代让引擎在容器里改代码，用一个**可执行的场景脚本**判定对错，通过了就把基线 commit 前移一位，没通过就把这次的补丁和日志写回下一次的提示词。

v3 相比 v2 的改动：LayerFS 已删除，工作区换成 git，分支 A/B 合并为一条落地路径；Gate 0 探针全部取消（它们探测的接口已不存在）；验收从白盒测试改为黑盒场景。

## 1. 已验证的前提

| # | 事实 | 位置 | 后果 |
| --- | --- | --- | --- |
| 1 | `EventKind` 是封闭 serde 枚举，无 `#[serde(other)]` | `event.rs:126-256`；未知 tag → `StoreError::Corrupt`，`spawn-postgres/src/lib.rs:94-97` | **RSI 状态只能进表，不能进事件日志**。写一个 `rsi.*` 事件会让下一次 `tick` 退出 1 |
| 2 | `spawn_append`、`submit`、`claim`、`finish` 都是 INVOKER；只有 `spawn_gateway_insert` 与 `spawn_read_*` 是 SECURITY DEFINER | `schema.sql` | 受限角色必须改造成 definer + 调用方白名单 |
| 3 | `tick` 把本进程的 `SPAWN_DATABASE_URL` 注入终端 | `main.rs:178-181` | 终端和控制器同权，agent 能伪造事件。必须新增 `SPAWN_TERMINAL_DATABASE_URL` |
| 4 | `spawn_grants` 无种子行；视图的 public select 已撤销 | `schema.sql:200`、`:549-552` | 控制器必须有 principal 与 grant 行 |
| 5 | 工作区是 `<branch>:<绝对目录>`，导出树不含 `.git` | `git.rs` | agent 读不到历史、其他分支和控制器的 ref |
| 6 | `commit` 用临时索引 + `write-tree` 比对父树，无改动返回 `Unchanged` | `git.rs` | 空改动天然被拒，不需要额外判据 |

## 2. 架构

`spawn evolve` 是一次性进程，推进一步就退出；常驻时由 `spawn rsi-loop` 循环调用，跟舰队的 `spawn loop`（Bridge、Observer、supervisor）分属两个进程、两个 systemd 单元。分开的原因：门禁一跑 20 分钟，不能卡住 Bridge；RSI 的 HALT（退出码 2）只停 RSI，不停舰队。

```text
spawn loop     --repo <bare> --root <ws>   # 舰队，永不自停
spawn rsi-loop --repo <bare> --root <ws>   # RSI 控制器，HALT 时退出 2
```

cron 方式仍可用（`*/1 * * * * spawn evolve ...`）：HALT 是粘性的，每次调用在开新迭代之前先检查，只有人清零计数器才会继续。

`spawn evolve` 每次：取 `pg_try_advisory_xact_lock`（单飞）→ 从 `rsi_*` 表重建状态 → 推进一步 → 退出。不折叠事件日志推导阶段。

| 组件 | 职责 |
| --- | --- |
| `spawn tick` | 不变 |
| `spawn evolve` | 选任务、开迭代、收补丁、跑门禁、前移基线、停 |
| `scripts/rsi/gate.sh` | 在主机上跑场景脚本，输出一行 JSON（舰队和 RSI 共用的唯一门禁） |
| `rsi/backlog/NNN.json` | 任务定义，**agent 不可写** |
| `rsi/scenarios/NNN.sh` | 黑盒场景，**agent 不可写** |

## 3. 状态机

阶段存 `rsi_iterations.phase`，转移条件由表计算。

| 阶段 | 转移条件 | 动作 |
| --- | --- | --- |
| `Idle` | 无 `phase ∉ {Landed, Rejected, Aborted}` 的迭代 | 见 3.1 |
| `Starting` | 有 Start 命令 | 见 3.2 |
| `Working` | execution 活跃，无完成标记 | 无动作 |
| `Proposing` | `.spawn/done` 存在 | 读补丁 → 写 `rsi_iterations.patch` → `submit(Stop)` |
| `Gating` | 补丁已记录 | 跑门禁 |
| `Landed` / `Rejected` / `Aborted` | 终态 | 无 |

**3.1 Idle 原子。** 写 `rsi_iterations` 与 `spawn_submit(Start)` 在同一个 SQL 事务里。分两步做，中间被杀会留下无命令的悬空迭代。

**3.2 Starting 以命令状态为准。** `agent_tasks.succeeded=false` 或 `state='finished'` 即失败，重试一次后 `Rejected`。`type_prompt` 超时也会返回 `Failed`，但 `execution.started` 已写（`bridge.rs:481-493`），只看"有无 execution"会误判。

**3.3 完成标记，不是 `turn.stopped`。** `turn.stopped` 只表示本轮模型停止。提示词要求 agent 结束时写 `.spawn/done`；没有该文件就提交 `Input` 继续。否则多回合任务在第一回合就被 Stop。

**3.4 控制器自己抓屏。** 不读 `screen.observed` 的历史值 —— 它只在 Bridge 打字时写入，等待期间不刷新。新增宿主侧采样命令，在决策前读一次。

## 4. 黑盒验收（核心）

**判据是一个可执行的场景脚本，不是一个测试名字。**

场景脚本把 `spawn` 当作黑盒驱动，只看外部可观测的事实：数据库行、pane 内容、退出码、文件系统状态。

```bash
#!/bin/sh
# rsi/scenarios/007.sh — 非 Ready 时 Input 必须被拒绝并重试
set -e
export SPAWN_DATABASE_URL=$RSI_TEST_DB
export SPAWN_GIT_REPO=$RSI_REPO
export SPAWN_WORKSPACE_ROOT=$RSI_WS
$SPAWN_BIN init-db
$SPAWN_BIN submit --agent agent_rsi --session session_rsi --key k1 \
  --action '{"type":"start","engine":"claude","prompt":"占位"}'
$SPAWN_BIN tick --repo "$RSI_REPO" --root "$RSI_WS"
$SPAWN_BIN submit --agent agent_rsi --session session_rsi --key k2 \
  --action '{"type":"input","text":"再来一次"}'
out=$($SPAWN_BIN tick --repo "$RSI_REPO" --root "$RSI_WS")
echo "$out" | grep -q 'commands=1' || exit 1
psql "$RSI_TEST_DB" -tAc \
  "select count(*) from spawn_events where event_name='command.finished'" | grep -qx 2
```

**门禁流程**：

```text
baseline_tree := git archive <base>
candidate_tree := baseline_tree + candidate.patch

1. 场景在 baseline_tree 上跑        必须 失败（红）
2. 场景在 candidate_tree 上跑       必须 通过（绿）
3. candidate.patch 非空且路径 ⊆ allowed_paths
4. 树大小与文件数在限内
```

第 1 步是黑盒验收最容易被跳过的一步，也是唯一能挡住"空改动通过"的判据。`cargo test` 在零匹配时退出 0，`grep` 在无匹配时退出 1，两者方向相反 —— 场景脚本必须显式断言红在前、绿在后。

**白盒全部不进判据。** `cargo test --workspace` 可以跑，但它的失败只记为警告，不决定 verdict。理由：白盒测的是实现形状，而 RSI 要改变形状。

代价必须写下来：没有白盒测试，回归只在场景覆盖的范围内被发现。每个任务因此必须自带一个场景，且场景数量只能增不能减（见 7）。

## 5. 隔离

| 容器 | LLM 代理 | `spawn` 二进制 | tmux | 测试数据库 |
| --- | --- | --- | --- | --- |
| agent 容器 | 需要 | 需要（hooks 调 `spawn hook`） | 需要 | 不需要 |
| 门禁容器 | 不需要 | **不需要** | 不需要 | 需要 |

门禁容器不给 `spawn` 二进制，是关键的一条：给了等于让候选代码直接调 `spawn hook` 伪造事件。

**现状（未实现容器）：门禁在宿主上以控制器同一 uid 运行。** 已做的缓解：拒绝改 `rsi/`、`scripts/rsi/`、任何 `build.rs` 和 `.cargo/`；ratchet 基准在基线的 manifest 下构建；裁决只取门禁 stdout 的最后一行，指标只取裁决 JSON 里的 `metric` 字段；门禁子进程自成进程组，结束或超时整组杀掉；输出文件放在每次新建的 0700 目录。剩余风险：同 uid 的候选代码仍能经 `/proc` 读写控制器进程的文件和环境、改控制器 checkout。真正的边界是门禁容器。

**不给 `SYS_ADMIN`，不用 FUSE，不用 overlay。** 实测 `--device /dev/fuse --cap-add SYS_ADMIN` 在 `overlay2` 存储驱动上挂 overlay 返回 `Invalid argument`；且 `SYS_ADMIN` 接近宿主 root，抵消隔离。

**代理网络。** agent 容器要连宿主环回口的 ccv42 代理，`rsi.md` v2 的"不挂 host network"因此作废。两种做法：`--network host`，或把代理绑到 docker0 并加 `--add-host`。默认取前者，配置项留出后者。

**工作区路径。** 容器把宿主目录挂到**同一绝对路径**，`Launch.cwd` 在两侧含义一致，不需要任何翻译（`git.rs:37-51`）。

## 6. 落地

基线是**一个 commit id**，落地是**一次写入**。

```text
开迭代:  base = rsi_state.baseline_commit
结束:    candidate = git commit-tree(candidate_tree) -p base
PASS:    CAS(base → candidate)；下一个任务从新 base 分支
FAIL:    丢弃补丁，写 rsi_attempts
```

`commit-tree` 由控制器在宿主侧执行，**不读 agent 写的任何 commit id**。v2 里从 `artifact.committed` 事件取 id 的做法已废弃 —— 那个事件可被伪造，且无改动时根本不写。

被拒迭代的工作树由 `unmount` 直接删除，没有分支需要 GC。

## 7. 记忆与复合

三张表，随 `schema.sql` 由 `init-db` 安装。

```sql
create table rsi_state (
    id               integer primary key check (id = 1),
    baseline_commit  text not null,
    iteration        integer not null default 0,
    consecutive_rejected integer not null default 0
);

create table rsi_iterations (
    iteration   integer primary key,
    task_id     integer not null,
    phase       text not null,
    patch       text,
    updated_at_ms bigint not null
);

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

**复合三件，缺一则退化为 PR 机器人**：

1. **红在前、绿在后**（§4 第 1、2 步）。挡住空改动与恒真判据。
2. **失败回灌。** 被拒后把 `diff`、门禁日志尾部、分类写入 `rsi_attempts`；下一次尝试的 prompt = 任务 prompt + 全部历史尝试摘要。
3. **场景数量不降。** 每轮统计 `rsi/scenarios/` 的文件数，少于上一轮即 `HALT`。

## 8. 安全

**8.1 终端与控制器分权。** 新增 `SPAWN_TERMINAL_DATABASE_URL`，`tick` 只把它注入终端（`main.rs:178-181`）。

| 角色 | 持有 |
| --- | --- |
| `spawn_controller` | `spawn_submit`/`claim`/`finish`、`rsi_*` 读写、`spawn_grants` 的 `*` |
| `spawn_terminal` | 只写 hook 来源事件 |

**8.2 写函数改 definer + 白名单（已实现，见 `schema.sql` 的 `spawn_append`）。** `spawn_append` 等是 INVOKER，给表权限等于给全部能力。改成 `SECURITY DEFINER` + 固定 `search_path`，按 `session_user` 校验 `(source, event_name)`。`spawn_terminal` 不能写 `observation.recorded`、`execution.ended`、`artifact.committed`。只授 execute 的做法会产生 42501，且被 `main.rs:263-272` 静默吞掉。

**8.3 控制器的读模型不可伪造。** 阶段从表计算，但 `agent_status`、`agent_executions` 是事件投影。若终端角色能写 `observation.recorded`，agent 就能伪造 `WAITING`。8.2 的白名单是防线。

**8.4 落地后的残留风险。** 候选落地后，下一轮的 `tick` 与 `evolve` 由落地代码构建。黑盒场景只证明被覆盖的行为正确，不证明无恶意。缓解只能是人审抽样。

## 9. 停止条件

| 条件 | 阈值 | 动作 |
| --- | --- | --- |
| 门禁拒绝 | 1 | Rejected |
| 连续 Rejected | 3 | HALT |
| 连续 Aborted（基础设施失败） | 3 | HALT |
| `main` 与记录的基线不一致（迭代之间） | 1 | HALT |
| 单迭代墙钟 | 45 分钟 | Aborted |
| 单迭代工具调用 | 60 | Aborted |
| 门禁墙钟 | 20 分钟 | Aborted |
| 场景文件数下降 | 1 | HALT |
| 累计迭代 | 可配置 | Idle 停 |

HALT 是粘性的：`spawn evolve` 在开新迭代之前先检查，重启不会多跑一轮。门禁通过后先把裁决写进迭代（`Accepted`），再移动 `main`，再在一个事务里移动记录的基线、标记 `Landed`、写入 attempt；两步都幂等，中途崩溃由下一次调用补完，不会重跑门禁。

基础设施失败必须与候选失败分开，否则一个死掉的代理会把好补丁判成 Rejected；但基础设施连续 3 次也要 HALT，否则候选制造 infra 失败就能绕过停止条件。

## 10. 阶段

| 阶段 | 内容 | 工期 | 验收 |
| --- | --- | --- | --- |
| 1 | 单个硬编码任务，真实 agent 在容器内改代码，场景脚本判红→绿，基线前移 | 4 天 | 一次真实迭代落地。**这是整个方案的否证点** |
| 2 | `spawn evolve` 状态机 + 原子 Idle + 完成标记 + 自抓屏 | 5 天 | 假 engine 连跑 3 次迭代，含 1 PASS 1 Rejected |
| 3 | 角色分离 + 函数白名单 + `SPAWN_TERMINAL_DATABASE_URL` | 3 天 | 终端角色伪造 `observation.recorded` 被拒 |
| 4 | 反馈回灌 + 场景计数守卫 | 3 天 | 同一任务第二次尝试的 prompt 含第一次的 diff 与门禁日志 |
| 5 | 连续 20 轮 | 3 天 | 无连续 3 次 Rejected，场景数不降，无空产出 |
| 6 | meta 双钥匙 | 2 天 | 一条 meta 变更走完三条 |

阶段 1 必须用真实 agent。用假 engine 跑通状态机只证明调度对，不证明方案可行。

## 11. 不做什么

- **不写 `rsi.*` 事件。** 封闭枚举，写进去产品链路就停。
- **不把白盒测试当判据。** 白盒测形状，RSI 改形状。
- **不从事件里取 commit id。** 事件可伪造，且无改动时不写。
- **不做不可重启的 daemon。** `spawn loop`（Bridge、Observer、supervisor 存活规则）和 `spawn rsi-loop`（`evolve`）各是一个进程，状态全在库里或事件日志里，杀掉即重启，不丢工作。拉起交给 systemd（`deploy/systemd/`），单元直接运行 `spawn`，不经包装脚本：`KillMode=process` 只杀主进程，主进程若是脚本，旧的 `spawn loop` 会留下来跟新的并跑。哪些部分能边跑边换、哪些必须停机，见 [rsi-upgrade.md](rsi-upgrade.md)。
- **不做并行种群。** 门禁是 pass/fail，选不出更好的那个。
- **不让 agent 改 backlog、场景或门禁脚本。**
- **不用 FUSE。** 内核的 overlay 在这个存储驱动上挂不上，`SYS_ADMIN` 又抵消隔离；`git archive` 已经够用。
- **不在工作区里 build。** 源码树 424 KiB，`target/` 694 MiB；build 在门禁容器内做。
- **不假设 Claude 权限标志存在。** 当前 `claude_arguments`（`bridge.rs:717-732`）带 `--allow-dangerously-skip-permissions`，该标志的语义由 `claude --help` 确认过："Recommended only for sandboxes with no internet access"。
