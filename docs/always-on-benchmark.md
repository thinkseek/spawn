# Always-On Production Benchmark Suite & Go/No-Go Scorecard

`scripts/bench-always-on.sh` validates the Spawn Always-On architecture under a kernel-enforced constrained production envelope (**1 CPU / 1 GiB RAM, PostgreSQL 16 `max_connections=30`**) across four dimensions with fail-closed anti-fake-green gates before long-running deployment.

## Architecture, Dimensions & Anti-Fake-Green Enforcement

- **Dimension A — D6 Storage Engine (`crates/spawn-host/examples/storage_benchmark.rs`)**
  - Uses heterogeneous, high-entropy content per cycle (`fill_mixed_payload` alternating seeded PRNG binary blocks and per-cycle headers) so `dedup_ratio` reflects real content-defined chunking rather than repeating identical static payloads.
  - Exercises `d6` (`CAS + Merkle + PG ref ledger`) with L1 tmpfs upperdirs (`--tmpfs-root`, `--tmpfs-budget-mb`), per-worker PG pool reuse (`--pg-pool-size 12`), and FastCDC oversized chunk indexing (`> 4 MiB`).
  - Measures genuine multi-thread concurrent `land_main_snapshot` CAS contention behind a `Barrier::new(concurrency)` (`--land-branches`), computing real per-landing closures (new Merkle tree nodes + snapshot envelope + file blobs + FastCDC chunks + `refs.journal` line) via `measure_landing_closure_bytes` and verifying every merged deliverable in `main_snapshot()`.
  - Executes a real **multi-process SIGKILL + torn-journal + PG16 restart drill** (`--crash-restart-drill`): spawns `--internal-crash-writer`, waits for `>= 6` flushed commits, sends `SIGKILL` (`signal 9`), appends a partial/corrupt torn tail line to `refs.journal` and verifies `CasMerkleStore` ignores/heals the tail without dropping committed entries, spawns `--internal-crash-verify`, and (under `bench-always-on.sh --keep`) stops and restarts PG16 before running `--internal-crash-verify` a second time in a fresh process against restarted PG16.
- **Dimension B — PostgreSQL 16 Control Plane (`crates/spawn-host/examples/always_on_control_plane_bench.rs`)**
  - **B1 (`xid8` EventLog concurrency)**: concurrent writer pool (`8` writers, `16` sessions, `1,024` events) appends while a polling `EventLog::read_global` cursor checks `1,024/1,024` delivery, strict per-session `(position, session_seq)` ordering, zero missing/unexpected/duplicate events, and zero PG deadlocks (`concurrent_commit_id_inversions` observed live), followed by `4` deterministic multi-transaction `(commit_xid, id)` inversion proofs (`deterministic_commit_id_inversions >= 4`) where `T1` reserves lower `id`, `T2` appends and commits higher `id` first, `read_global` advances past `T2`, and `T1` then commits with higher `commit_xid` and is recovered with zero misses or duplicates.
  - **B2 (`BridgeShard` claim contention)**: `4` `BridgeShard::Partition` workers contend on `256` queued commands, verifying `owns_session` routing, zero double-claims, 100% completion across `claim_for_shard` + `finish`, and zero deadlocks.
- **Dimension C — Bridge-Driven Context & Work-Hours Attribution (`always_on_control_plane_bench.rs`)**
  - Drives `16` ephemeral tasks end-to-end through real `Bridge::work()` / `Bridge::poll()` (`MemoryStore` + `LocalTranscriptArchive` + PG `cost_record` / `cost_window` + `scorecard_facts` + `ProvChangeInput`) without synthetic duration delays (`duration_ms > 0` from real Bridge execution timestamps).
  - Verifies cross-table lineage (`agent_id`, `execution_id`, `task_id`, `model_label`, disk `transcript_ref` SHA-256 + `ingest::claude_usage`, `duration_ms > 0`, `cost_usd > 0` / `unpriced_tokens == 0`, and `task.landed`), plus an automated **8-case negative falsification check** (`negative_cases_checked == 8`) proving every attribution field gap forces failure.
- **Dimension D — Fail-Closed Resource-Envelope Orchestration & Scorer Self-Test (`scripts/bench-always-on.sh`)**
  - Spawns PG 16 under **1 CPU / 1 GiB RAM** (`docker` `--cpus=1 --memory=1g` or `systemd-run --user --scope` `CPUQuota=100% MemoryMax=1G MemorySwapMax=0`) with `max_connections=30`, `shared_buffers=128MB`, `work_mem=4MB`, `maintenance_work_mem=32MB`, `effective_cache_size=512MB`, `fsync=on`, `synchronous_commit=on`, `log_lock_waits=on`.
  - Reads `/proc/<postmaster_pid>/cgroup` + `systemctl --user show` (or `docker inspect`) to verify actual kernel cgroup limits (`cgroup_enforced=true`); `--pg-mode local` or missing cgroups are marked `UNCONSTRAINED` and strictly fail `NO-GO`.
  - Polls `/proc` RSS (`VmRSS` sum) and `pg_stat_activity` / `pg_stat_database` every 500 ms plus synchronous start/stop samples, logging any `psql` failure as `{"ok":false}` and failing `NO-GO` if `samples < 3`, `probe_errors > 0`, `peak_pg_rss_mib == 0`, `pg_version_num < 160000`, or `max_connections != 30`.
  - Ships `--self-test` running **18 negative-falsification cases** directly through `evaluate_scorecard(...)`.

## Go/No-Go Gates (12/12 Required)

| Gate ID | Dimension | Metric | Red-Line SLO |
| :--- | :--- | :--- | :--- |
| `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` & `8/8` verified |
| `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` & `16/16` verified |
| `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `dedup_ratio` | `>= 2.00x` |
| `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `dedup_ratio` | `>= 2.00x` |
| `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` |
| `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` |
| `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms` & `n=16` |
| `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `SIGKILL + torn_heal + pg_restart` | `0/0/0`, `SIGKILL=true`, `torn_heal=true`, `pg_restart=true` |
| `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0`, `write_errors=0`, `xid8_reorders>=4 (det>=4)` |
| `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0` and full claim |
| `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `samples / cgroup / pg16 / rss / conn / deadlocks` | `samples>=3`, `probe_err=0`, `PG>=16`, `max_conn=30`, `1C/1G cgroup`, `conn<=27`, `RSS<900MiB` |
| `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%`, `gaps=0`, `unpriced=0`, `bridge=true`, `neg=true` |

## Running the Suite

Both benchmark binaries read `SPAWN_BENCH_DATABASE_URL` pointing to a disposable PostgreSQL 16 database (never `SPAWN_DATABASE_URL`) and automatically drop their disposable schemas on exit via `BenchSchemaGuard`.

```bash
# Verify scorer negative-falsification suite (18/18 fail-closed checks)
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

- **Generated (UTC)**: `2026-11-28T05:25:19Z`
- **Verdict**: **GO** (**12/12** gates passed)
- **Source Revision**: `511e88c5ce020089f15b31d907768686a529b204`
- **Host / PG16 Envelope**: `x86_64` (192 cores host), PG `16.16` in `systemd` mode — `1 CPU / 1 GiB RAM (systemd-v1 cfs=100000/100000us, memory.limit_in_bytes=1073741824B, MemorySwapMax=0)` (`max_connections=30`)
- **PG16 Monitor (`48` samples, `0` probe errors)**: peak `14/30` connections (active peak `11`), RSS peak `511.20 MiB`, PG deadlocks delta `0`

| Status | Gate | Dimension | Metric | Red-Line SLO | Measured (N252) |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **PASS** | `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB) & 8/8 verified` | `215,619 B (0.206 MiB), landed=8/8, verified=8` |
| **PASS** | `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB) & 16/16 verified` | `215,996 B (0.206 MiB), landed=16/16, verified=16` |
| **PASS** | `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `dedup_ratio` | `>= 2.00x` | `5.40x` |
| **PASS** | `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `dedup_ratio` | `>= 2.00x` | `7.92x` |
| **PASS** | `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` | `71.42 ms` |
| **PASS** | `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` | `181.27 ms` |
| **PASS** | `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms & n=16` | `258.57 ms (n=16, retries=93)` |
| **PASS** | `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `SIGKILL + torn_heal + pg_restart` | `0/0/0, SIGKILL=true, torn_heal=true, pg_restart=true` | `8b=(0/0/0), 16b=(0/0/0), pg_restart=True (snaps=23)` |
| **PASS** | `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0, write_errors=0, xid8_reorders>=4 (det>=4)` | `0 / 0 / 0 / 0 (1024/1024 events, xid8_reorders=6 [det=4])` |
| **PASS** | `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0 and full claim` | `0 / 0 / 0 / 0 (256/256)` |
| **PASS** | `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `samples / cgroup / pg16 / rss / conn / deadlocks` | `samples>=3, probe_err=0, PG>=16, max_conn=30, 1C/1G cgroup, conn<=27, RSS<900MiB` | `14/30 conns, 511.20 MiB RSS, 0 deadlocks, samples=48` |
| **PASS** | `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%, gaps=0, unpriced=0, bridge=true, neg=true` | `100.00% (16/16), gaps=0, bridge=True, neg=True, dur_ms=787, usd=0.7320` |

### Measured Dimension Breakdown (N252)

- **Dimension A · D6 (8 concurrent branches)**: `checkpoint_changed` p50=`19.59 ms` / p95=`71.42 ms` / p99=`76.12 ms` (`n=96`); `merge_base` p50=`57.18 ms` / p95=`92.29 ms` (`n=16`); `land_main` p50=`61.32 ms` / p95=`181.39 ms` (`n=8`, `cas_retries=25`); `dedup=5.40x` (`49,152,000 B` logical / `9,098,093 B` CAS); mean/max land delta=`215,454` / `215,619 B`; crash-restart recovery=`169.15 ms` (`SIGKILL=true`, `torn_heal=true`, `0/0/0` loss across `15` snapshots / `307` blobs).
- **Dimension A · D6 (16 concurrent branches)**: `checkpoint_changed` p50=`91.60 ms` / p95=`181.27 ms` / p99=`197.03 ms` (`n=192`); `merge_base` p50=`143.74 ms` / p95=`184.07 ms` / p99=`187.85 ms` (`n=32`); `land_main` p50=`100.36 ms` / p95=`258.57 ms` (`n=16`, `cas_retries=93`); `dedup=7.92x` (`98,304,000 B` logical / `12,416,435 B` CAS); mean/max land delta=`215,643` / `215,996 B`; crash-restart recovery=`177.34 ms` (`SIGKILL=true`, `torn_heal=true`) plus fresh-process post-PG16-restart verification (`147.24 ms`, `0/0/0` loss across `23` snapshots / `475` blobs).
- **Dimension B · `xid8` Watermark Stress**: `1,024/1,024` events across `16` sessions / `8` writers at `385.7 events/s`; `append` p50=`3.68 ms` / p95=`86.80 ms` / p99=`93.81 ms` (`n=1024`); `read_global` p50=`6.69 ms` / p95=`94.08 ms` / p99=`100.04 ms` (`n=76`); `6` total `(commit_xid, id)` reorderings (`2` concurrent + `4` deterministic multi-transaction) verified with `0` session OOO, `0` dupes, `0` misses, `0` unexpected reads, and `0` deadlocks.
- **Dimension B · 4-Shard Bridge Claim**: `256/256` commands claimed at `117.1 claims/s`; `claim` p50=`4.04 ms` / p95=`86.47 ms` / p99=`93.83 ms` (`n=256`); `finish` p50=`2.60 ms` / p95=`84.95 ms` / p99=`95.77 ms` (`n=256`); balanced partition counts `{"shard_0": 64, "shard_1": 64, "shard_2": 64, "shard_3": 64}`.
- **Dimension C · Bridge-Driven Attribution**: `16/16` tasks (`100.00%`), `bridge_driven=true`, `negative_falsification_passed=true` (`8` cases), field gaps=`0` (`agent_id`, `execution_id`, `task_id`, `model_label`, `transcript_ref`, `duration_ms`, `cost_or_tokens`, `landing_record`), work duration total=`787 ms`, tokens=`243,200` (`unpriced=0`), cost=`$0.7320`.
