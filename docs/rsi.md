# Spawn RSI

结论：RSI 是一个目标任务（objective mission），没有单独的控制器。`spawn integrate` 从 `rsi/backlog` 取编号最小的未完成目标，交给常驻 worker，一次一个任务；黑盒门禁 `scripts/rsi/gate.sh` 判定；通过就在宿主侧提交，用 CAS 推送前移任务源的 `main`；被拒就带着门禁输出退回 worker。状态全在事件日志的控制流里，任务与落地的规则见 [fleet-architecture.md](fleet-architecture.md) §10。

## 1. 组成

| 部分 | 位置 | 作用 |
| --- | --- | --- |
| 目标 | `rsi/backlog/NNN.json` | `id`、`prompt`，可选 `title`；循环目标另有 `recurring`、`metric`、`unit`、`direction`、`target` |
| 目标场景 | `rsi/scenarios/NNN.sh` | 判定一个目标的黑盒脚本 |
| 检查场景 | `rsi/scenarios/check-*.sh` | 每次落地都必须通过：clippy、测试、e2e、黑盒、web 构建 |
| 构建钩子 | `rsi/scenarios/build` | 在被测树里构建 `spawn` |
| 受保护路径 | `rsi/scenarios/protected` | 候选不能改的路径 |
| 门禁 | `scripts/rsi/gate.sh` | 判定候选，输出一行 JSON 裁决；舰队与 RSI 共用 |
| 门禁自测 | `scripts/rsi/test-gate.sh` | 门禁本身的测试 |
| 积分器 | `spawn integrate`（`crates/spawn-cli/src/integrate.rs`、`objectives.rs`） | 派目标、跑门禁、落地、停机 |

backlog、场景和门禁都从控制器的 checkout 按绝对路径读取，agent 改不到。

## 2. 一个目标怎样落地

1. 目标任务没有未完成的 task 时，积分器给常驻 worker 派下一个未完成的目标（`task.assigned`，带目标编号与场景路径）。一次性目标落地一次即完成；循环目标直到最近一次落地的指标达到 `target` 才完成。
2. dispatcher 投递 brief：目标的 prompt、工作规则（只在工作区里工作，不跑 git、不提交，不改受保护路径），以及最近几次尝试的门禁裁决。
3. worker 结束时写 `.spawn/report.md`。验收方式为 `gate`，报告直接进门禁，不经 supervisor：supervisor 不会收到这类报告，`spawn task accept|reject` 也拒绝它们。
4. 积分器取 worker 推送的分支，构建候选（`main` 动过就与它合并），跑门禁；先记 `task.gated`（写明它评判的报告版本 `revision`，任务已换了版本就被拒），再 CAS 推送，最后记 `task.landed`。
5. 被拒：任务回到 `revise`，门禁输出（最多 4000 字符）作为说明。
6. 超预算（分钟数或工具调用数）：记 `task.aborted`，停掉 worker，计一次中止。

## 3. 黑盒验收

**判据是一个可执行的场景脚本，不是一个测试名字。** 场景把 `spawn` 当黑盒驱动，只看外部可观测的事实：输出、退出码、数据库行、文件。例如 `rsi/scenarios/001.sh`：

```sh
[ -x "${SPAWN_BIN:-}" ] || exit 2
output=$("$SPAWN_BIN" version) || exit 1
[ "$(printf '%s\n' "$output" | grep -c '^spawn ')" -eq 1 ] || exit 1
```

退出码约定：0 通过，1 断言失败，其他是基础设施错误（缺工具、连不上数据库、超时）。

**红在前、绿在后。** 任务的场景必须在基线上以 1 失败，在候选上通过。这一步挡住空改动和恒真判据。基线结果在任何候选代码运行之前判定，按（基线 commit、场景、构建钩子、门禁脚本）缓存；判定为 1 需要两次一致的运行，其他退出码让门禁中止，不给裁决。

**其他场景。** `check-*` 是不变式，必须通过。标了 `# rsi: ratchet` 的场景和基线上也失败的目标场景跳过：没人解决的目标不算回归。

**候选自己的测试和 lint 也判它。** `check-tests` 与 `check-clippy` 必须通过；可运行的测试数不得少于基线，基线的每个测试名在候选上仍须可运行（改名、移到别的模块路径都算删）；diff 新增或改动的测试函数体不得为空（`{}`、只有注释或 `;`；`assert!(true)` 这类不算空，查不出）；lint 配置不得改（`[lints]` 表、`clippy.toml`、新增的 crate 级 `#![allow]`、删掉的 `#![deny]`）。任务的判据仍是它的场景，测试只防回归。

**指标。** 候选的任务运行在 stdout 恰好打印一行 `METRIC` 时，裁决带上它；积分器只从裁决里读指标，不读场景输出。

代价：回归只在场景和测试覆盖的范围内被发现。所以每个目标自带一个场景，场景数也不能降（停机规则，见 §5）。

## 4. 隔离

**门禁沙箱。** 候选控制的一切（构建钩子、场景、检查、对候选清单的 `cargo metadata`）都在 bubblewrap 里运行：

- 根目录只读，`/tmp` 私有，新的 PID 命名空间（`/proc` 里看不到门禁和积分器）；
- 被测树是唯一可写路径，基线树只读；`/run`（含用户运行时目录、docker socket、系统总线）、`/var/tmp`、宿主的 `TMPDIR` 是空 tmpfs；启动时在沙箱里扫一遍仍可 connect 的 Unix socket 并逐个遮住：除测试库那一个 socket 文件外，宿主的 socket 一个都连不到（只读挂载挡不住 connect）；
- `$HOME` 是空 tmpfs，只读绑回 Rust 工具链、node/npm 的安装目录、控制器的 `scripts` 与 `web/node_modules`；
- 新的网络命名空间，只有回环：线上数据库、`spawn serve`、Jev 中继都连不到；测试数据库只经一个 Unix socket 进入：socket 地址只绑那一个 `.s.PGSQL.<port>` 文件，TCP 地址（127.0.0.1、localhost、[::1]、主机名）由门禁在沙箱外转发到一个 socket；
- 环境变量清空到白名单；cargo 用私有 `CARGO_HOME`，注册表缓存只读、离线构建；
- 裁决写到积分器自己的目录（沙箱看不到），积分器只信那个文件，且只在门禁退出 0 时信。

积分器在传测试库地址之前按服务器身份核对：测试库与 owner 的库是同一个服务器，或核对失败时，不传。

**剩余风险。** 沙箱与积分器同一个 uid，没有用户命名空间。门禁还拒绝改 `rsi/`、`scripts/rsi/`、`scripts/blackbox/`、`scripts/e2e.sh`、任何 `build.rs`、`.cargo/` 和 Cargo.toml 里的 `build` 键。落地只移动任务源的 `main`；线上二进制只在 owner 运行 `deploy/deploy.sh` 时更换。黑盒场景只证明被覆盖的行为正确，不证明无恶意，缓解只能是人审抽样。

**agent 一侧。** RSI 的常驻 worker 是普通 worker，受同一 uid 的限制（fleet-architecture.md §8）。

**不给 `SYS_ADMIN`，不用 FUSE，不用 overlay。** 工作区由 `git archive` 导出，已经够用。实测 `--device /dev/fuse --cap-add SYS_ADMIN` 在 `overlay2` 存储驱动上挂 overlay 返回 `Invalid argument`；而且 `SYS_ADMIN` 接近宿主 root，抵消隔离。

## 5. 落地与停机

基线是**一个 commit id**：任务源的 `main`。落地是**一次 CAS 推送**（`--force-with-lease` 指向基线）。积分器在宿主侧提交工作区，**不读 agent 写的任何 commit id**。

| 条件 | 阈值 | 动作 |
| --- | --- | --- |
| 门禁拒绝 | 1 | 任务回到 `revise` |
| 连续拒绝 | 3 | 停机 |
| 连续中止（门禁基础设施失败、超时、超预算） | 3 | 停机 |
| 场景数低于下限 | 1 | 停机 |
| 源的 `main` 不在上次落地留下的位置 | 1 | 停机 |
| 门禁墙钟 | 45 分钟 | 中止 |
| 单任务墙钟、工具调用 | 任务的预算（RSI 任务：45 分钟、60 次） | 中止 |
| 补丁大小 | 1 MiB | 拒绝 |

停机按任务记录在日志里，是粘性的：停机的任务什么都不落地，直到人运行 `spawn mission resume`。恢复后，下一次门禁把源的 `main` 记为新基线，把场景数记为新下限。

基础设施失败与候选失败分开计数，否则一个死掉的代理会把好补丁判成被拒；但连续 3 次中止也停机，否则候选制造基础设施失败就能绕过停止条件。

裁决先写进日志，再移动 `main`，再记落地；中途崩溃时下一轮补完落地，不重跑门禁。推送时 `main` 已经动过，裁决记为 `stale`，任务重新判定；但候选已在 `main` 的历史里（上一次推送成功而落地未记），就记为落地。同一个源上有别的任务处于 `landing` 时，其他任务等它落定，不据此时的 `main` 停机。拉取源失败时本轮跳过该任务，不拿镜像里旧的 `main` 当源的 `main`。

## 6. 复合

缺一则退化为 PR 机器人：

1. **红在前、绿在后**（§3）。挡住空改动与恒真判据。
2. **失败回灌。** 被拒的任务带着门禁输出回到 worker；下一个任务的 brief 带着该目标最近几次尝试的裁决。
3. **场景数不降。** 每次门禁统计 `rsi/scenarios/` 里的 `.sh` 文件数，少于下限即停机。

## 7. 不做什么

- **不加新的事件种类。** `EventKind` 是封闭枚举；任务与落地都是 `control.recorded` 事件，类型在 detail 里。
- **不从事件里取 commit id。** 积分器在宿主侧提交工作区。
- **不做不可重启的 daemon。** `spawn loop`（Bridge、Observer、派发、存活规则）与 `spawn integrate`（门禁与落地）各是一个进程，状态全在事件日志里，杀掉即重启，不丢工作。拉起交给 systemd（`deploy/systemd/`：`spawn-loop`、`spawn-integrate`、`spawn-serve`、`spawn-jev-relay`、`spawn-model-relay`、`spawn-backup.timer`）；运行 Spawn 的单元直接执行 `spawn`，不经包装脚本：`KillMode=process` 只杀主进程，主进程若是脚本，被替换的 `spawn loop` 会留下来跟新的并跑。哪些部分能边跑边换、哪些必须停机，见 [upgrading.md](upgrading.md)。
- **不做并行种群。** 门禁是 pass/fail，选不出更好的那个；一个目标任务一次只有一个任务在做。
- **不让 agent 改 backlog、场景或门禁脚本。**
- **不在 agent 的工作区里判定。** 门禁把基线与候选导出到自己的临时目录里构建和运行。
- **不假设 Claude 权限标志存在。** `claude_arguments`（`crates/spawn-core/src/bridge.rs`）带 `--allow-dangerously-skip-permissions`，该标志的语义由 `claude --help` 确认过："Recommended only for sandboxes with no internet access"。
