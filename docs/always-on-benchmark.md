# Always-On Production Benchmark Suite & Go/No-Go Scorecard

`scripts/bench-always-on.sh` validates the Spawn Always-On architecture under a constrained production envelope (**1 CPU / 1 GiB RAM, PostgreSQL 16 `max_connections=30`**) across four dimensions before long-running deployment.

## Architecture & Dimensions

- **Dimension A — D6 Storage Engine (`crates/spawn-host/examples/storage_benchmark.rs`)**
  - Exercises `d6` (`CAS + Merkle + PG ref ledger`) with L1 tmpfs upperdirs (`--tmpfs-root`, `--tmpfs-budget-mb`) at 8 and 16 parallel agent branches (`--land-branches`, `--pg-pool-size 12`).
  - Measures `checkpoint_changed`, `merge_base`, and `land_main` (`land_main_snapshot`) latency (`p50`/`p95`/`p99`), CAS deduplication (`dedup_ratio = logical_written_bytes / cas_stored_bytes`), per-landing CAS delta (`bytes_per_landing` / `max_bytes_per_landing`), and FastCDC oversized chunk indexing (`> 4 MiB`).
  - Executes a full crash-restart zero-loss verification (`--crash-restart-drill`): re-opens a fresh `CasMerkleStore`, `PgRefLedger`, and `D6Workspace`, replays `refs.journal`, checks `main` + all branches, and streams every blob via `Store::stream_blob`.
- **Dimension B — PostgreSQL 16 Control Plane (`crates/spawn-host/examples/always_on_control_plane_bench.rs`)**
  - **B1 (`xid8` EventLog concurrency)**: concurrent writer pool appends events across sessions while a polling `EventLog::read_global` cursor checks `640/640` delivery, strict per-session `(position, session_seq)` order, zero missing/unexpected/duplicate events, and zero PG deadlocks while logging cross-session `(commit_xid, id)` commit reorderings (`commit_id_inversions`).
  - **B2 (`BridgeShard` claim contention)**: 4 `BridgeShard::Partition` workers contend on queued commands, verifying `owns_session` routing, zero double-claims, 100% completion across `claim_for_shard` + `finish`, and zero deadlocks.
- **Dimension C — Context & Work-Hours Attribution (`always_on_control_plane_bench.rs`)**
  - Executes 16 synthetic tasks across alternating engines (`claude` with preset/model telemetry and `codex` with direct `--model-label`) and verifies `task.landed` scorecard facts, provenance lineage (`ProvChangeInput`), `execution.started`/`execution.ended` duration (`duration_ms > 0`), `LocalTranscriptArchive` byte readback, and `cost_record` / `cost_window` pricing (`unpriced_tokens == 0`).
- **Dimension D — Resource-Envelope Orchestration & Scorecard (`scripts/bench-always-on.sh`)**
  - Spawns PG 16 under **1 CPU / 1 GiB RAM** (`docker`, `systemd-run --user --scope`, or `local` fallback) with `max_connections=30`, `shared_buffers=128MB`, `work_mem=4MB`, `maintenance_work_mem=32MB`, `effective_cache_size=512MB`, `fsync=on`, `synchronous_commit=on`, `log_lock_waits=on`.
  - Polls `/proc` RSS (or `docker stats`) and `pg_stat_activity` / `pg_stat_database` deadlocks every 500 ms.
  - Emits `scorecard.json` and `scorecard.md` evaluating 12 deterministic Go/No-Go gates.

## Go/No-Go Gates (12/12 Required)

| Gate ID | Dimension | Metric | Red-Line SLO |
| :--- | :--- | :--- | :--- |
| `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` |
| `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` |
| `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `dedup_ratio` | `>= 2.00x` |
| `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `dedup_ratio` | `>= 2.00x` |
| `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` |
| `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` |
| `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms` |
| `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `lost_refs / lost_commits / corrupt_objects` | `0 / 0 / 0` across 8 & 16 branches |
| `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0` and `write_errors=0` |
| `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0` and full claim |
| `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `peak_conn / peak_rss_mib / pg_deadlocks` | `<= 27` conns, `< 900.0 MiB` RSS, `0` deadlocks |
| `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%`, `gaps=0`, `unpriced=0` |

## Running the Suite

Both direct benchmark binaries read `SPAWN_BENCH_DATABASE_URL` pointing to a disposable PostgreSQL 16 database (never `SPAWN_DATABASE_URL`) and create isolated temporary schemas per run.

```bash
# Full orchestrated suite under systemd 1C/1G envelope (or --pg-mode docker)
bash scripts/bench-always-on.sh --out-dir /tmp/always-on-bench --pg-mode auto

# Direct D6 benchmark with shared PG pool, L1 tmpfs, FastCDC, land-branches, and crash drill
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" \
cargo run --release -p spawn-host --example storage_benchmark -- \
  --backend d6 --concurrency 16 --pg-pool-size 12 \
  --tmpfs-root /dev/shm/spawn-bench-l1 --tmpfs-budget-mb 2048 \
  --iterations 12 --warmup 2 --files 128 --file-bytes 4096 --edits 4 \
  --oversized-file-bytes 5242880 --land-branches --crash-restart-drill

# Direct control-plane + attribution benchmark
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" \
cargo run --release -p spawn-host --example always_on_control_plane_bench -- \
  --sessions 16 --events-per-session 40 --shards 4 \
  --commands-per-session 16 --writer-pool 8 --attribution-tasks 16
```

## N252 Baseline Scorecard (1 CPU / 1 GiB RAM PG16 Envelope)

- **Generated (UTC)**: `2026-11-28T04:25:33Z`
- **Verdict**: **GO** (**12/12** gates passed)
- **Source Revision**: `1aac7b9e40dcee591016fe81ca26cc23953bb633`
- **Host / PG16 Envelope**: `x86_64` (192 cores host), PostgreSQL `16.16` in `systemd` 1C/1G mode (`max_connections=30`, `CPUQuota=100%`, `MemoryMax=1G`, `MemorySwapMax=0`)
- **PG16 Monitor (`31` samples)**: peak `14/30` connections (active peak `10`), RSS peak `459.62 MiB`, PG deadlocks delta `0`

| Status | Gate | Dimension | Metric | Red-Line SLO | Measured (N252) |
| :--- | :--- | :--- | :--- | :--- | :--- |
| **PASS** | `D6_8_LAND_DELTA` | A: D6 8-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` | `1,328 B (0.001 MiB)` |
| **PASS** | `D6_16_LAND_DELTA` | A: D6 16-Branch | `max_bytes_per_landing` | `< 52,428,800 B (50.0 MiB)` | `1,776 B (0.002 MiB)` |
| **PASS** | `D6_8_DEDUP_RATIO` | A: D6 8-Branch | `dedup_ratio` | `>= 2.00x` | `8.64x` |
| **PASS** | `D6_16_DEDUP_RATIO` | A: D6 16-Branch | `dedup_ratio` | `>= 2.00x` | `17.13x` |
| **PASS** | `D6_8_CHECKPOINT_P95` | A: D6 8-Branch | `checkpoint_changed p95_ms` | `<= 500.00 ms` | `83.88 ms` |
| **PASS** | `D6_16_CHECKPOINT_P95` | A: D6 16-Branch | `checkpoint_changed p95_ms` | `<= 1000.00 ms` | `206.24 ms` |
| **PASS** | `D6_16_LAND_P95` | A: D6 16-Branch | `land_main p95_ms` | `<= 500.00 ms` | `11.62 ms` |
| **PASS** | `D6_CRASH_RESTART_ZERO_LOSS` | A: Crash-Restart | `lost_refs / lost_commits / corrupt_objects` | `0 / 0 / 0` across 8 & 16 branches | `8b=(0/0/0), 16b=(0/0/0)` |
| **PASS** | `PG16_XID8_WATERMARK_INTEGRITY` | B: 1C/1G PG16 | `session OOO / dup / miss / deadlock` | `0 / 0 / 0 / 0` and `write_errors=0` | `0 / 0 / 0 / 0` (`640/640`, `xid8_reorders=6`) |
| **PASS** | `PG16_SHARD_CLAIM_INTEGRITY` | B: 1C/1G PG16 | `dup / unclaimed / wrong_shard / deadlock` | `0 / 0 / 0 / 0` and full claim | `0 / 0 / 0 / 0` (`256/256`) |
| **PASS** | `PG16_RESOURCE_ENVELOPE` | B: 1C/1G PG16 | `peak_conn / peak_rss_mib / pg_deadlocks` | `<= 27 conns, < 900.0 MiB RSS, 0 deadlocks` | `14/30 conns, 459.62 MiB RSS, 0 deadlocks` |
| **PASS** | `ATTRIBUTION_COMPLETENESS` | C: Attribution | `completeness_pct & field_gaps` | `100.00%, gaps=0, unpriced=0` | `100.00% (16/16), gaps=0, dur_ms=188, usd=0.5512` |

### Measured Dimension Breakdown (N252)

- **Dimension A · D6 (8 concurrent branches)**: `checkpoint_changed` p50=`25.71 ms` / p95=`83.88 ms` / p99=`96.16 ms` (`n=96`), `merge_base` p50=`59.13 ms` / p95=`105.17 ms` (`n=16`), `land_main` p50=`6.97 ms` / p95=`9.55 ms` (`n=8`), dedup=`8.64x`, mean/max land delta=`1,136 / 1,328 B`, crash-restart recovery=`157.55 ms` (`0` loss across `9` snapshots / `70` blobs).
- **Dimension A · D6 (16 concurrent branches)**: `checkpoint_changed` p50=`100.85 ms` / p95=`206.24 ms` / p99=`222.39 ms` (`n=192`), `merge_base` p50=`128.51 ms` / p95=`193.70 ms` / p99=`199.54 ms` (`n=32`), `land_main` p50=`7.59 ms` / p95=`11.62 ms` (`n=16`), dedup=`17.13x`, mean/max land delta=`1,358 / 1,776 B`, crash-restart recovery=`155.78 ms` (`0` loss across `17` snapshots / `78` blobs).
- **Dimension B · `xid8` Watermark Stress**: `640/640` events across `16` sessions / `8` writers at `349.5 events/s`; append p50=`3.88 ms` / p95=`90.39 ms` / p99=`109.02 ms` (`n=640`); `read_global` p50=`6.42 ms` / p95=`94.51 ms` / p99=`101.48 ms` (`n=49`); `6` cross-session `(commit_xid, id)` commit reorderings handled with `0` session OOO, `0` dupes, `0` misses, `0` unexpected reads, and `0` deadlocks.
- **Dimension B · 4-Shard Bridge Claim**: `256/256` commands claimed at `123.6 claims/s`; claim p50=`4.05 ms` / p95=`83.15 ms` / p99=`90.51 ms`; finish p50=`2.69 ms` / p95=`85.58 ms` / p99=`95.67 ms`; balanced partition counts `{"shard_0": 64, "shard_1": 64, "shard_2": 64, "shard_3": 64}`.
- **Dimension C · Attribution**: `16/16` tasks (`100.00%`), field gaps=`0` (`agent_id`, `execution_id`, `task_id`, `model_label`, `transcript_ref`, `duration_ms`, `cost_or_tokens`, `landing_record`), work duration total=`188 ms`, tokens=`243,200` (`unpriced=0`), cost=`$0.5512`.
