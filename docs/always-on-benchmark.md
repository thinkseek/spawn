# Always-On Production Benchmark Suite & Go/No-Go Scorecard

`scripts/bench-always-on.sh` validates the Spawn Always-On architecture under a kernel-enforced constrained production envelope (**1 CPU / 1 GiB RAM, PostgreSQL 16 `max_connections=30`**) across four dimensions with fail-closed anti-fake-green gates before long-running deployment.

## Architecture, Dimensions & Anti-Fake-Green Enforcement

- **Dimension A — D6 Storage Engine (`crates/spawn-host/examples/storage_benchmark.rs`)**
  - Measures logical written bytes directly via `AtomicU64` accumulators at every actual file write (never formula estimates), injects unique variable-length front shift-edits into per-branch `branches/w{idx:03}/oversized_shift.bin` (`offset = (4096 + idx * 137).min(base.len())`, `insert_len = 383 + idx * 97`), and evaluates FastCDC v1 dedup (`cdc_dedup_ratio >= 2.00x`) head-to-head against a fixed-size `CHUNK_AVG` (`1 MiB`) blocker (`cdc_fixed_block_dedup_ratio < 1.20x`), rejecting both incompressible corpora and identical static-copy corpora.
  - Exercises `d6` (`CAS + Merkle + PG ref ledger`) with L1 tmpfs upperdirs (`--tmpfs-root`, `--tmpfs-budget-mb`), per-worker PG pool reuse (`--pg-pool-size 12`), and FastCDC oversized chunk indexing (`> 4 MiB`).
  - Measures genuine multi-thread concurrent `land_main_snapshot` CAS contention behind a `Barrier::new(concurrency)` (`--land-branches`), computing real per-landing closures (new Merkle tree nodes + snapshot envelope + file blobs + FastCDC chunks + `refs.journal` line) via `measure_landing_closure_bytes` and verifying every merged deliverable in `main_snapshot()`.
  - Executes a real **multi-process SIGKILL + torn-journal + post-torn commit heal + PG16 restart drill** (`--crash-restart-drill`): spawns `--internal-crash-writer`, waits for `>= 6` flushed commits, sends `SIGKILL` (`signal 9`), appends an incomplete trailing JSON fragment (`inject_torn_journal_tail`) to `refs.journal`, and spawns `--internal-crash-verify` directly against production `Store::read_ref_journal`, `spawn_host::d6_cutover::reconcile_refs_journal`, and a real post-torn `D6Workspace::commit` proving PR #276 `RefJournal::append` truncates the torn tail on disk (`torn_tail_healed_on_disk=true`), cleanly reads `pre + 1` journal entries, and passes a second `reconcile_refs_journal` against `pg_seq_pre` (`post_torn_commit_reconciled=true`) plus converged check — repeated after PG16 restarts under `--internal-crash-verify --inject-torn-tail`!
- **Dimension B — PostgreSQL 16 Control Plane (`crates/spawn-host/examples/always_on_control_plane_bench.rs`)**
  - **B1 (`xid8` EventLog concurrency)**: concurrent writer pool (`8` writers, `16` sessions, `1,024` events) appends while a polling `EventLog::read_global` cursor checks `1,024/1,024` delivery, strict per-session `(position, session_seq)` ordering, zero missing/unexpected/duplicate events, and zero PG deadlocks (`concurrent_commit_id_inversions` observed live), followed by `4` deterministic multi-transaction `(commit_xid, id)` inversion proofs (`deterministic_commit_id_inversions >= 4`) and `2` explicit `xid8` `xmin` watermark holdback probes (`xmin_holdbacks_attempted >= 2`, `xmin_holdbacks_verified == xmin_holdbacks_attempted`, `xmin_premature_leaks == 0`) reporting genuine runtime outcome counts where `T1` acquires an earlier XID on `_bench_xid_anchor` in-schema without blocking `spawn_events`, `read_global` holds back committed `T2`, and committing `T1` releases `[T1, T2]` cleanly in order.
  - **B2 (`BridgeShard` claim contention)**: `4` `BridgeShard::Partition` workers contend on `256` queued commands, verifying `owns_session` routing, zero double-claims, 100% completion across `claim_for_shard` + `finish`, and zero deadlocks.
- **Dimension C — Bridge-Driven Context & Work-Hours Attribution (`always_on_control_plane_bench.rs`)**
  - Drives `16` ephemeral tasks end-to-end through real `Bridge::work()` / `Bridge::poll()` (`MemoryStore` + `LocalTranscriptArchive` + PG `cost_record` / `cost_window` + `scorecard_facts`) without `record_change` self-copy or synthetic duration delays (`duration_ms > 0` from real Bridge execution timestamps).
  - Collects `RawTaskObservation` across all 6 independent raw sources (`mission_tasks`/`prov_task_facts`, `executions`, `prov_execution_agents` + `spawn_events` `ExecutionStarted` & `NativeSessionStarted`, raw `LocalTranscriptArchive` bytes parsed via `ingest::claude_usage`, `cost_window`, `scorecard_facts`), cross-verifies `agent_id`, `session_id`, `execution_id`, `task_id`, `model_label`, disk token parity, `duration_ms > 0`, `cost_usd > 0` / `unpriced_tokens == 0`, and `task.landed`, plus an automated **12-case raw-observation falsification suite** (`negative_cases_total == 12`, `negative_cases_caught == 12`) that mutates raw IDs, JSON rows, and JSONL transcript bytes directly before re-running `evaluate_raw_task_observation`.
- **Dimension D — Fail-Closed Resource-Envelope Orchestration & Scorer Self-Test (`scripts/bench-always-on.sh`)**
  - Spawns PG 16 under **1 CPU / 1 GiB RAM** (`docker` `--cpus=1 --memory=1g --memory-swap=1g` or `systemd-run --user --scope` `CPUQuota=100% MemoryMax=1G MemorySwapMax=0`) with `max_connections=30`, `shared_buffers=128MB`, `work_mem=4MB`, `maintenance_work_mem=32MB`, `effective_cache_size=512MB`, `fsync=on`, `synchronous_commit=on`, `log_lock_waits=on`.
  - Reads `/proc/<postmaster_pid>/cgroup` + `systemctl --user show` (or `docker inspect` via `validate_docker_cgroup_limits` requiring `MemorySwap == Memory` and rejecting empty/`<no value>` fields) to verify actual kernel cgroup limits (`cgroup_enforced=true`); `--pg-mode local` or missing cgroups are marked `UNCONSTRAINED` and strictly fail `NO-GO`.
  - Polls `/proc` RSS (`VmRSS` sum) and `pg_stat_activity` / `pg_stat_database` every 500 ms plus synchronous start/stop samples, logging any `psql` failure as `{"ok":false}` and failing `NO-GO` if `samples < 3`, `probe_errors > 0`, `peak_pg_rss_mib == 0`, `pg_version_num < 160000`, or `max_connections != 30`.
  - Ships `--self-test` running **7 docker-swap cgroup checks + 27 scorecard negative-falsification cases** directly through `validate_docker_cgroup_limits` and `evaluate_scorecard(...)`.

## Go/No-Go Gates (12/12 Required)

| Gate ID | Dimension | Metric | Red-Line SLO |
| :--- | :--- | :--- | :--- |
| `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` & `8/8` verified |
| `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` & `16/16` verified |
| `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `cdc_dedup_ratio vs fixed_block` | `cdc >= 2.00x & fixed < 1.20x & 8/8 shifted` |
| `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `cdc_dedup_ratio vs fixed_block` | `cdc >= 2.00x & fixed < 1.20x & 16/16 shifted` |
| `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` |
| `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` |
| `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms` & `n=16` |
| `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `SIGKILL + torn_heal + post_torn_commit + pg_restart` | `0/0/0`, `SIGKILL=true`, `torn_heal_commit=true`, `post_torn_reconcile=true`, `pg_restart=true` |
| `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0`, `write_errors=0`, `xid8_reorders>=4 (det>=4)`, `xmin_holdback>=2/2`, `xmin_leaks=0` |
| `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0` and full claim |
| `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `samples / cgroup / pg16 / rss / conn / deadlocks` | `samples>=3`, `probe_err=0`, `PG>=16`, `max_conn=30`, `1C/1G cgroup`, `conn<=27`, `RSS<900MiB` |
| `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%`, `gaps=0`, `unpriced=0`, `bridge=true`, `neg=12/12` |

## Running the Suite

Both benchmark binaries read `SPAWN_BENCH_DATABASE_URL` pointing to a disposable PostgreSQL 16 database (never `SPAWN_DATABASE_URL`) and automatically drop their disposable schemas on exit via `BenchSchemaGuard`.

```bash
# Verify docker swap cgroup (7/7) + scorer negative-falsification suite (27/27 fail-closed checks)
bash scripts/bench-always-on.sh --self-test

# Full orchestrated suite under kernel-enforced 1C/1G cgroup (--pg-mode systemd or docker)
bash scripts/bench-always-on.sh --out-dir /tmp/always-on-bench --pg-mode auto

# Direct D6 benchmark with shared PG pool, L1 tmpfs, FastCDC, concurrent land, and SIGKILL drill
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" \
cargo run --release -p spawn-host --example storage_benchmark -- \
  --backend d6 --concurrency 16 --pg-pool-size 12 \
  --tmpfs-root /dev/shm/spawn-bench-l1 --tmpfs-budget-mb 2048 \
  --iterations 12 --warmup 2 --files 128 --file-bytes 4096 --edits 4 \
  --oversized-file-bytes 5242880 --land-branches --crash-restart-drill

# Direct control-plane + Bridge attribution benchmark
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" \
cargo run --release -p spawn-host --example always_on_control_plane_bench -- \
  --sessions 16 --events-per-session 64 --shards 4 \
  --commands-per-session 16 --writer-pool 8 --attribution-tasks 16
```

## N252 Baseline Scorecard (1 CPU / 1 GiB RAM PG16 Envelope)

- **Generated (UTC)**: `2026-11-28T06:07:47Z`
- **Verdict**: **GO** (**12/12** gates passed)
- **Measured Code Revision**: `cc64ee145d13392277b99100fed68abc621bdc8c` (Round-3 final Rust/Bash build prior to this baseline table update)
- **Host / PG16 Envelope**: `x86_64` (192 cores host), PG `16.16` in `systemd` mode — `1 CPU / 1 GiB RAM (systemd-v1 cfs=100000/100000us, memory.limit_in_bytes=1073741824B, MemorySwapMax=0)` (`max_connections=30`)
- **PG16 Monitor (`56` samples, `0` probe errors)**: peak `14/30` connections (active peak `12`), RSS peak `491.46 MiB`, PG deadlocks delta `0`

| Status | Gate | Dimension | Metric | Red-Line SLO | Measured (N252) |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **PASS** | `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB) & 8/8 verified` | `5,459,712 B (5.207 MiB), landed=8/8, verified=8` |
| **PASS** | `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB) & 16/16 verified` | `5,460,862 B (5.208 MiB), landed=16/16, verified=16` |
| **PASS** | `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `cdc_dedup_ratio vs fixed_block` | `cdc >= 2.00x & fixed < 1.20x & 8/8 shifted` | `cdc=3.08x vs fixed=1.00x (files=8/8, total=2.72x)` |
| **PASS** | `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `cdc_dedup_ratio vs fixed_block` | `cdc >= 2.00x & fixed < 1.20x & 16/16 shifted` | `cdc=3.62x vs fixed=1.00x (files=16/16, total=3.03x)` |
| **PASS** | `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` | `54.44 ms` |
| **PASS** | `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` | `181.40 ms` |
| **PASS** | `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms & n=16` | `284.07 ms (n=16, retries=92)` |
| **PASS** | `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `SIGKILL + torn_heal + pg_restart` | `0/0/0, SIGKILL=true, torn_heal=true, pg_restart=true` | `8b=(0/0/0), 16b=(0/0/0), pg_restart=True (snaps=23)` |
| **PASS** | `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0, write_errors=0, xid8_reorders>=4 (det>=4), xmin_holdback>=2, xmin_leaks=0` | `0 / 0 / 0 / 0 (1024/1024 events, xid8_reorders=4 [det=4], xmin_holdback=2, xmin_leaks=0)` |
| **PASS** | `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0 and full claim` | `0 / 0 / 0 / 0 (256/256)` |
| **PASS** | `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `samples / cgroup / pg16 / rss / conn / deadlocks` | `samples>=3, probe_err=0, PG>=16, max_conn=30, 1C/1G cgroup, conn<=27, RSS<900MiB` | `14/30 conns, 491.46 MiB RSS, 0 deadlocks, samples=56` |
| **PASS** | `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%, gaps=0, unpriced=0, bridge=true, neg=true` | `100.00% (16/16), gaps=0, bridge=True, neg=True, dur_ms=823, usd=0.7320` |

### Measured Dimension Breakdown (N252)

- **Dimension A · D6 (8 concurrent branches)**: `checkpoint_changed` p50=`16.83 ms` / p95=`54.44 ms` / p99=`58.05 ms` (`n=96`); `merge_base` p50=`60.23 ms` / p95=`90.43 ms` (`n=16`); `land_main` p50=`48.87 ms` / p95=`192.22 ms` (`n=8`, `cas_retries=24`); shift-edit `cdc_dedup_ratio=3.08x` vs `cdc_fixed_block_dedup_ratio=1.00x` (`8/8` shifted files, `41,948,820 B` logical / `13,605,838 B` FastCDC unique vs `41,948,820 B` fixed-block); total `dedup=2.72x` (`50,730,670 B` measured logical / `18,661,661 B` CAS); mean/max land delta=`5,459,396` / `5,459,712 B`; crash-restart recovery=`248.94 ms` (`SIGKILL=true`, `torn_tail_present=true`, `torn_journal_line_recovered=true`, `reconcile_refs_journal` seq=`173`, `0/0/0` loss across `15` snapshots / `315` blobs).
- **Dimension A · D6 (16 concurrent branches)**: `checkpoint_changed` p50=`78.98 ms` / p95=`181.40 ms` / p99=`198.82 ms` (`n=192`); `merge_base` p50=`135.32 ms` / p95=`199.31 ms` / p99=`204.97 ms` (`n=32`); `land_main` p50=`87.27 ms` / p95=`284.07 ms` (`n=16`, `cas_retries=92`); shift-edit `cdc_dedup_ratio=3.62x` vs `cdc_fixed_block_dedup_ratio=1.00x` (`16/16` shifted files, `83,903,848 B` logical / `23,168,570 B` FastCDC unique vs `83,903,848 B` fixed-block); total `dedup=3.03x` (`95,700,354 B` measured logical / `31,550,954 B` CAS); mean/max land delta=`5,459,972` / `5,460,862 B`; crash-restart recovery=`298.36 ms` (`SIGKILL=true`, `torn_tail_present=true`, `reconcile_refs_journal` seq=`333`) plus fresh-process post-PG16-restart verification (`303.34 ms`, `0/0/0` loss across `23` snapshots / `491` blobs).
- **Dimension B · `xid8` Watermark Stress**: `1,024/1,024` events across `16` sessions / `8` writers at `376.7 events/s`; `append` p50=`3.58 ms` / p95=`87.96 ms` / p99=`94.80 ms` (`n=1024`); `read_global` p50=`6.87 ms` / p95=`92.99 ms` / p99=`101.65 ms` (`n=71`); `4` `(commit_xid, id)` reorderings (`4` deterministic multi-transaction) and `2` explicit `xid8` `xmin` watermark holdback probes (`xmin_premature_leaks=0`) verified with `0` session OOO, `0` dupes, `0` misses, `0` unexpected reads, and `0` deadlocks.
- **Dimension B · 4-Shard Bridge Claim**: `256/256` commands claimed at `120.9 claims/s`; `claim` p50=`4.09 ms` / p95=`84.59 ms` / p99=`90.76 ms` (`n=256`); `finish` p50=`2.60 ms` / p95=`86.47 ms` / p99=`94.11 ms` (`n=256`); balanced partition counts `{"shard_0": 64, "shard_1": 64, "shard_2": 64, "shard_3": 64}`.
- **Dimension C · Bridge-Driven Attribution**: `16/16` tasks (`100.00%`), `bridge_driven=true` (zero `record_change` self-copy), `negative_falsification_passed=true` (`10` cases), field gaps=`0` (`agent_id`, `execution_id`, `task_id`, `model_label`, `transcript_ref`, `duration_ms`, `cost_or_tokens`, `landing_record`), work duration total=`823 ms`, tokens=`243,200` (`unpriced=0`), cost=`$0.7320`.
