# Storage benchmark

For a large, real-agent fleet case, see **Agent cluster golden benchmark** below.
That case deliberately uses the live event log and starts model-backed agents;
the single-process benchmark below is the isolated microbenchmark.

Spawn's storage benchmark is an end-to-end exercise of the host adapters, not a
synthetic disk-speed test. It creates an isolated repository and workspace, then
repeatedly performs and validates:

- a checkpoint after real file edits (`checkpoint_changed`);
- a no-op checkpoint against the kept index (`checkpoint_noop`);
- a transcript append, including `fdatasync`, followed by a final content check
  (`transcript_append`);
- optional PostgreSQL `host.storage` event writes and reads through the real
  `PostgresStore` (`postgres_append`, `postgres_read`);
- optional unmount/mount cycles and branch-loss recovery (`unmount`, `mount`,
  and `checkpoint_changed` with `Commit::Recreated`).

Every run ends with `git fsck --full --no-dangling`. The JSON report contains
sample counts, min, p50, p95, p99, max and mean latency, whole-cycle throughput,
and apparent bytes/files for the repository, workspace and transcript archive.
Progress goes to stderr, so stdout can be redirected directly to a report file.
The checkpoint path requires Git's `--pathspec-from-file` support (Git 2.25 or
later); `environment.git_version` records the executable that produced each
report. Do not compare reports produced by different Git versions.

## Run

```sh
cargo build --release -p spawn-host --example storage_benchmark

target/release/examples/storage_benchmark \
  --source-revision "$(git rev-parse HEAD)" \
  --iterations 100 --warmup 10 \
  --files 512 --file-bytes 4096 --edits 4 \
  --transcript-bytes 16384 --remount-every 10 \
  > storage-baseline.json
```

The default scratch root is a unique directory under `TMPDIR`. `--root` may name
another path, but it must not exist: the benchmark never reuses or cleans an
existing directory. It removes only the directory it created. `--keep` preserves
it for inspection. Use a root on the filesystem whose performance you want to
measure.

Run `--help` for all sizes and cycle controls. Two focused integrity scenarios
are useful after changing checkpoint code:

```sh
# A file over the cap stays out of every checkpoint.
target/release/examples/storage_benchmark \
  --iterations 20 --files 64 --file-bytes 1024 --edits 2 \
  --oversized-file-bytes 2048 > storage-oversized.json

# Delete the branch before every third checkpoint; the store must recreate it.
target/release/examples/storage_benchmark \
  --iterations 20 --files 64 --file-bytes 1024 --edits 2 \
  --recreate-every 3 > storage-recreate.json
```

An oversized file is intentionally not preserved by unmount, so the benchmark
rejects combining those two options rather than silently losing that coverage.
Every seed, changed and no-op checkpoint also runs `git ls-tree` and fails if
`oversized.bin` entered the committed tree.

Every report records `environment.spawn_source_revision` from the explicit
option and a SHA-256 of the running benchmark executable. The comparison tool
requires one identity per group and rejects comparing a binary/source identity
with itself. Supply the product revision being measured; keep the benchmark
harness identical on both sides.

## Compare repeated runs

Use at least three independent runs when judging an optimization. Do not compare
one baseline run with one candidate run and do not mix configurations.

```sh
python3 -I scripts/storage-benchmark-compare.py \
  --baseline baseline-1.json baseline-2.json baseline-3.json \
  --candidate candidate-1.json candidate-2.json candidate-3.json
```

The comparison uses the median across runs of each operation's p50 and p95. It
requires identical operation sets, configurations and Git versions across every
report; a missing candidate operation cannot disappear from the regression
check. It also compares whole-cycle throughput and exits 1 on a median-p50 or
throughput regression over 10% (`--max-regression-percent` changes that bound). Latency
improvements are negative percentages; throughput improvements are positive.
Small differences in transcript, mount and unmount timings are usually noise:
the checkpoint optimization does not touch those operations.

## PostgreSQL

`SPAWN_BENCH_DATABASE_URL` opts into the database benchmark. It must name a
**dedicated, disposable benchmark database or cluster**. The benchmark creates a
unique schema and never reads `SPAWN_DATABASE_URL`, but an explicitly supplied
URL is otherwise obeyed. Do not point it at the live store.

The URL is read from the environment, is not included in the report, and should
not be placed on a command line. The benchmark leaves its schema behind; use a
disposable cluster or have the owner remove that schema afterward. Database
writes and reads are included in whole-cycle time when enabled.

For a local isolated cluster, use PostgreSQL with no TCP listener, a private
Unix socket directory and its normal durable settings. Do not copy the test
clusters' `fsync=off` setting into a durability benchmark.

The benchmark also supports D6 native and dual-track shadow backends (implemented in
[`crates/spawn-host/examples/storage_benchmark.rs`](../crates/spawn-host/examples/storage_benchmark.rs))
and cross-backend comparison in `scripts/storage-benchmark-compare.py`:

- `--backend <git|d6|shadow>`: storage backend to measure (`git` default; `d6` and `shadow` require `SPAWN_BENCH_DATABASE_URL`).
- `--concurrency <n>`: concurrent branch worker threads (`1` default).
- `--tmpfs-root <directory>`: enable bounded L1 `tmpfs` upperdir mounting for `d6`/`shadow`.
- `--tmpfs-budget-mb <n>`: L1 `tmpfs` budget in MiB when `--tmpfs-root` is set (default `2048`).
- `python3 -I scripts/storage-benchmark-compare.py --allow-cross-backend ...`: compares repeated `--release` reports across `git`, `d6`, and `d6` L1 `tmpfs`.

## D6 vs Git `--release` Measured Results (PR #146, Isolated PostgreSQL 16)

Measured in [`PR #146`](https://github.com/thinkseek/spawn/pull/146) against an isolated temporary PostgreSQL 16 instance (`40` iterations, `4` edits per cycle):

### Case A: 512 files (`c=1`) and Shadow Oracle Verification

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | cycle p50 / p95 (ms) | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git` | 2799.0 | 14.29 | 23.00 / 35.83 | 18.36 / 22.85 | 45.07 / 54.83 | 52.46 / 66.40 | 3,014,023 | 1.437 |
| `d6` (disk) | 2373.5 | 16.85 | 26.37 / 28.54 | 8.28 / 8.52 | 20.23 / 35.89 | 45.68 / 47.87 | 3,876,695 | 1.849 |
| `d6` (L1 `tmpfs`) | 1900.1 | 21.05 | 21.64 / 27.68 | 6.01 / 8.46 | 22.26 / 22.81 | 38.99 / 46.92 | 3,876,695 (`l1_reserved=2,097,178`) | 1.849 |
| `shadow` | 9402.7 | 4.25 | 103.67 / 107.97 | 81.51 / 86.27 | 73.77 / 84.75 | 195.23 / 204.07 | 4,058,207 | 1.935 |

Shadow oracle telemetry for Case A: `checkpoints_agreed=91`, `merge_agree_clean=4`, `merge_agree_conflict=0`, `tree_mismatches=0`, `tracked_set_leaks=0`, `shadow_errors=0`, `merge_blockers=0`, `has_divergences=false`.

### Case B: 2,048 files (`c=1` and `c=8`)

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | cycle p50 / p95 (ms) | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git` (`c=1`) | 6004.6 | 6.66 | 58.19 / 89.45 | 44.81 / 76.90 | 70.81 / 74.95 | 114.94 / 151.90 | 9,477,007 | 1.130 |
| `d6` (`c=1`, disk) | 3103.4 | 12.89 | 34.04 / 37.89 | 15.56 / 17.97 | 35.84 / 37.47 | 60.12 / 67.81 | 10,707,479 | 1.276 |
| `git` (`c=8`) | 7595.1 | 42.13 | 69.31 / 100.30 | 54.51 / 92.01 | 84.67 / 109.79 | 144.52 / 197.00 | 74,677,638 | 1.113 |
| `d6` (`c=8`, disk) | 4706.0 | 68.00 | 52.15 / 85.46 | 20.34 / 31.06 | 37.38 / 45.90 | 99.20 / 147.54 | 74,805,486 (`cas=1,797,660`) | 1.115 |
| `d6` (`c=8`, L1 `tmpfs`) | 4166.2 | 76.81 | 39.67 / 66.50 | 17.56 / 26.83 | 40.40 / 44.54 | 74.34 / 113.16 | 74,815,813 (`l1_reserved=67,109,072`, `cas=1,807,987`) | 1.115 |

Cross-backend comparison (`2048` files × `c=8`, `d6 L1 tmpfs` vs `git`): throughput `+85.8%`, `checkpoint_changed` p50 `-42.8%`, `checkpoint_noop` p50 `-67.8%`, `merge_base` p50 `-52.3%`.

### Case C: Oversized 110 MiB Asset (`--files 512 --iterations 10 --edits 4 --oversized-file-bytes 110000000`)

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | `oversized_ingested` | `oversized_chunked` | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: | ---: |
| `git` | 1168.2 | 8.56 | 27.52 / 34.97 | 20.32 / 25.77 | 43.87 / 46.53 | `false` | `—` | 112,452,571 | 1.003 |
| `d6` | 844.7 | 11.84 | 25.60 / 30.56 | 7.55 / 11.20 | 22.84 / 25.43 | `true` | `true` | 117,860,047 (`cas=5,516,985`) | 1.051 |

(Case C uses a uniform synthetic `oversized.bin` payload of `110,000,000` bytes so FastCDC deduplicates repeating chunks inside `<cas>/chunks` to ~5.5 MiB CAS overhead beside the checked-out workspace copy.)

## Latest `main` (`45093817`) `--release` D5 L1 `tmpfs` & Cross-Backend Results

Measured against `origin/main` revision `4509381764fb00c907c8827a28b50d373e4b1d9d` (`git version 2.52.0`, `--release`, offline build of [`crates/spawn-host/examples/storage_benchmark.rs`](../crates/spawn-host/examples/storage_benchmark.rs)) using an isolated disposable PostgreSQL 16 instance (normal durable settings via `SPAWN_BENCH_DATABASE_URL`) and a bounded `/dev/shm` L1 `tmpfs` upperdir (`--tmpfs-budget-mb 512`), with `40` measured iterations, `5` warmup cycles, `4` edits per cycle, `4,096` B/file, and `16,384` B transcript appends:

### 512 files (`c=1` and `c=8`, L1 `tmpfs` budget `512` MiB)

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | cycle p50 / p95 (ms) | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git` (`c=1`) | 2532.8 | 15.79 | 21.48 / 25.30 | 17.64 / 18.30 | 45.63 / 50.85 | 46.16 / 52.05 | 3,014,015 | 1.437 |
| `d6` (`c=1`, disk) | 1962.9 | 20.38 | 22.92 / 23.77 | 7.65 / 8.40 | 20.81 / 21.01 | 39.99 / 40.30 | 3,876,695 (`cas=1,042,087`) | 1.849 |
| `d6` (`c=1`, L1 `tmpfs` 512 MiB) | 2071.7 | 19.31 | 23.04 / 23.69 | 7.94 / 8.19 | 27.85 / 28.65 | 39.60 / 40.16 | 3,876,695 (`l1_reserved=2,097,178`, `cas=1,042,087`) | 1.849 |
| `shadow` (`c=1`) | 8394.2 | 4.77 | 86.71 / 123.96 | 77.48 / 90.80 | 66.69 / 81.06 | 173.66 / 217.60 | 4,059,893 (`cas=1,045,756`) | 1.936 |
| `git` (`c=8`) | 3498.7 | 91.46 | 25.67 / 42.85 | 20.08 / 36.51 | 52.06 / 65.39 | 57.60 / 86.85 | 23,220,047 | 1.384 |
| `d6` (`c=8`, disk) | 2717.9 | 117.74 | 21.76 / 25.31 | 8.30 / 8.95 | 30.27 / 33.77 | 50.31 / 57.66 | 23,886,524 (`cas=1,210,346`) | 1.424 |
| `d6` (`c=8`, L1 `tmpfs` 512 MiB) | 2747.5 | 116.47 | 21.29 / 24.02 | 7.87 / 8.52 | 30.42 / 38.84 | 48.10 / 56.08 | 23,870,108 (`l1_reserved=16,777,424`, `cas=1,193,930`) | 1.423 |
| `shadow` (`c=8`) | 10002.6 | 31.99 | 99.06 / 113.18 | 82.32 / 92.16 | 81.25 / 90.33 | 192.96 / 211.80 | 24,517,244 (`cas=1,290,103`) | 1.461 |

- **Shadow oracle parity (`c=1` and `c=8`)**:
  - `c=1`: `checkpoints_agreed=91`, `merge_agree_clean=4`, `merge_agree_conflict=0`, `tree_mismatches=0`, `tracked_set_leaks=0`, `shadow_errors=0`, `merge_blockers=0`, `has_divergences=false` (`git_fsck=passed`).
  - `c=8`: `checkpoints_agreed=728`, `merge_agree_clean=32`, `merge_agree_conflict=0`, `tree_mismatches=0`, `tracked_set_leaks=0`, `shadow_errors=0`, `merge_blockers=0`, `has_divergences=false` (`git_fsck=passed`).
- **Comparison highlights (`512` files)**:
  - At `c=8`, `d6` (`L1 tmpfs`, `512` MiB) improves over `git` (`c=8`) by `+27.3%` aggregate throughput (`116.47` vs `91.46` cyc/s), `-17.1%` / `-43.9%` `checkpoint_changed` p50/p95, `-60.8%` / `-76.7%` `checkpoint_noop` p50/p95, `-41.6%` `merge_base` p50, and `-16.5%` / `-35.4%` cycle p50/p95, while reserving `16,777,424` bytes inside the `512` MiB L1 `tmpfs` budget.
  - Compared with PR #146 Case A (`c=1`), `d6` (`disk`) improved from `16.85` to `20.38` cyc/s (`-17.3%` wall time), and `shadow` (`c=1`) improved from `4.25` to `4.77` cyc/s with zero divergences at both `c=1` and `c=8`.

### 2,048 files (`c=8` spot-check vs PR #146 Case B)

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | cycle p50 / p95 (ms) | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git` (`c=8`) | 7119.3 | 44.95 | 61.05 / 92.72 | 49.65 / 80.55 | 87.37 / 111.00 | 127.41 / 174.53 | 74,677,338 | 1.113 |
| `d6` (`c=8`, disk) | 3283.3 | 97.46 | 30.86 / 36.69 | 14.77 / 17.80 | 35.21 / 42.85 | 61.33 / 71.95 | 74,799,101 (`cas=1,791,275`) | 1.115 |

At `2,048` files (`c=8`), `d6` (`disk`) reached `97.46` cyc/s (`+116.8%` vs `git`'s `44.95` cyc/s; `-49.5%` `ck` p50, `-70.3%` `noop` p50, `-59.7%` `merge` p50, `-51.9%` `cycle` p50), up from `68.00` cyc/s in PR #146 Case B. On this revision (`45093817`), the `2,048`-file `d6` (`c=8`, L1 `tmpfs` `512` MiB) run hit the concurrent snapshot key race noted below on `3/3` attempts after finishing `40/40` cycles, so no row is tabulated for it here.

### Observed `c=8` Concurrency Caveat (`spawn_snapshots_git_oid_key`)

On `45093817`, `spawn_snapshot_record` in [`crates/spawn-postgres/sql/schema.sql`](../crates/spawn-postgres/sql/schema.sql) guards `INSERT INTO spawn_snapshots` with `ON CONFLICT (id) DO NOTHING`, which handles collisions on the primary key `id` but does not serialize or swallow concurrent unique conflicts on the secondary unique constraint `spawn_snapshots_git_oid_key` under PostgreSQL `READ COMMITTED`. When `storage_benchmark` runs `c=8` workers with identical synthetic payloads inside the same 1-second timestamp bucket, `d6` can intermittently fail with SQLSTATE `23505` (`spawn_snapshots_git_oid_key`), observed once on initial `d6` (`512`, `c=8`, disk) before passing cleanly on retry and `3/3` times on `d6` (`2,048`, `c=8`, L1 `tmpfs`). All values tabulated above come from clean passing runs (`integrity=passed`, `git_fsck=passed`, `shadow_oracle.has_divergences=false`); making `spawn_snapshot_record` idempotent across both `(id)` and `(git_oid)` is a tracked follow-up (`#97`/`#113`).

## Post-PR #190 (`0b0396d0`) `--release` `2,048`-file `c=8` Results

Measured on real Linux against revision `0b0396d02fd76776b0e3a69b96de6bc861e4ada9` using an isolated disposable PostgreSQL 16 instance (normal durable settings via `SPAWN_BENCH_DATABASE_URL`) and a bounded `/dev/shm` L1 `tmpfs` upperdir (`--tmpfs-budget-mb 512`), with `40` measured iterations, `5` warmup cycles, `4` edits per cycle, `4,096` B/file, and `16,384` B transcript appends:

| Config | Wall (ms) | Agg (cyc/s) | ck p50 / p95 (ms) | noop p50 / p95 (ms) | merge p50 / p95 (ms) | cycle p50 / p95 (ms) | Disk (B) | Amp |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `git` (`c=8`) | 7385.7 | 43.33 | 63.30 / 92.90 | 50.45 / 85.80 | 93.60 / 112.48 | 130.60 / 180.65 | 74,678,903 | 1.113 |
| `d6` (`c=8`, disk) | 3890.4 | 82.25 | 37.41 / 55.67 | 17.67 / 26.87 | 40.05 / 44.43 | 70.54 / 102.90 | 74,810,948 (`cas=1,803,122`) | 1.115 |
| `d6` (`c=8`, L1 `tmpfs` 512 MiB) | 3569.3 | 89.65 | 34.72 / 41.31 | 16.21 / 20.48 | 38.84 / 48.01 | 66.41 / 78.33 | 74,812,480 (`l1_reserved=67,109,072`, `cas=1,804,654`) | 1.115 |
| `shadow` (`c=8`) | 17447.5 | 18.34 | 173.63 / 201.89 | 156.79 / 182.72 | 116.67 / 141.40 | 345.27 / 383.02 | 76,516,496 (`cas=1,832,239`) | 1.140 |

All four runs completed with `integrity=passed` and zero SQLSTATE `23505` errors. The shadow oracle reported `checkpoints_agreed=728`, `merge_agree_clean=32`, `merge_agree_conflict=0`, `tree_mismatches=0`, `tracked_set_leaks=0`, `shadow_errors=0`, `merge_blockers=0`, and `has_divergences=false` (`git_fsck=passed`).

### Sanitized Reproduction Commands

```sh
cargo build --release --locked --offline -p spawn-host --example storage_benchmark

# Start an isolated disposable PostgreSQL 16 instance (durable settings) and export:
export SPAWN_BENCH_DATABASE_URL="postgres://<user>:<pass>@127.0.0.1:<port>/<db>"

# Run across git, d6 (disk), and d6 (L1 tmpfs) at c=1 and c=8:
for c in 1 8; do
  target/release/examples/storage_benchmark \
    --source-revision 4509381764fb00c907c8827a28b50d373e4b1d9d \
    --backend git --concurrency "$c" \
    --files 512 --file-bytes 4096 --iterations 40 --warmup 5 --edits 4 --transcript-bytes 16384 \
    > "bench-git-c${c}.json"

  target/release/examples/storage_benchmark \
    --source-revision 4509381764fb00c907c8827a28b50d373e4b1d9d \
    --backend d6 --concurrency "$c" \
    --files 512 --file-bytes 4096 --iterations 40 --warmup 5 --edits 4 --transcript-bytes 16384 \
    > "bench-d6-disk-c${c}.json"

  target/release/examples/storage_benchmark \
    --source-revision 4509381764fb00c907c8827a28b50d373e4b1d9d \
    --backend d6 --concurrency "$c" \
    --files 512 --file-bytes 4096 --iterations 40 --warmup 5 --edits 4 --transcript-bytes 16384 \
    --tmpfs-root "<shm_dir>/d6-l1-c${c}" --tmpfs-budget-mb 512 \
    > "bench-d6-l1-c${c}.json"
done
```

## Interpreting the report

This is a warm-cache, single-writer workload with deterministic, compressible
files. It is designed for before/after comparisons on one host, not as a promise
of fleet-wide throughput or a replacement for `fio`. Increase `--iterations` for
stability and use `--files`, `--edits` and `--remount-every` to model the actual
workload. Run competing systems or other heavy benchmarks separately.

`disk` values are apparent sizes, not physical allocation. Reflinks and sparse
files can make physical usage lower; use `scripts/storage-extents.py` for the
physical-extent reading. Transcript appends and PostgreSQL commits are durable;
the benchmark does not add an extra `fsck` or filesystem sync inside any timed
operation.

## Agent cluster golden benchmark

`scripts/storage-cluster-benchmark.py` measures the whole fleet path with real
Spawn missions, tasks, engines, checkpoints, event-log writes and transcript
handling. Unlike the isolated example, its `run` command deliberately uses the
configured live Spawn store and starts model-backed agents. Run it only when
that load and model usage are intended.

Generate the default deterministic golden repository: 32 shards, 64 files per
shard, 256 KiB per file, **512 MiB total**. Every file and shard has a SHA-256
in `golden/manifest.json`; the payload is a reproducible, incompressible hash
stream.

```sh
python3 -I scripts/storage-cluster-benchmark.py generate \
  --root /new/empty/storage-golden
```

The generator writes `seed/`, `golden.git/` and `golden-config.json`. Keep the
bare repository to reproduce or rerun the case. The generated repository's
`tools/golden_workload.py` is what agents execute; it reads and validates the
worker's shard, then writes a deterministic 1 MiB artifact and a JSON result
with durable file and directory syncs.

Start 32 real agents for eight iterations each:

```sh
SPAWN_BIN="$HOME/.local/share/spawn-rsi/bin/spawn" \
python3 -I scripts/storage-cluster-benchmark.py run \
  --source /new/empty/storage-golden/golden.git \
  --output /new/empty/storage-cluster-result \
  --agents 32 --iterations 8
```

The environment must already contain the normal Spawn owner configuration,
including `SPAWN_WORKSPACE_ROOT`. The runner checks that workspace filesystem
(not merely the golden source) for every mounted dataset plus a 20 GiB reserve,
and separately checks source space for published artifacts. By default the
controller drives **synchronized turns**:
the initial prompt runs iteration zero, and it sends the next input only after
every agent's previous `turn.stopped`, counted across the merged event history
rather than only the latest tail. Each measured turn must contain exactly one
workload Bash call and one result checkpoint; the timing analysis rejects extra
Bash calls instead of attaching the artifact to an unrelated command. An
additional finalization turn writes `.spawn/report.md`
only after all measured turns, keeping report generation out of iteration
latency. This forces a real turn boundary and checkpoint per iteration instead
of assuming that eight tool calls in one model turn necessarily produce eight
checkpoints. Agents may not modify the golden inputs or tools, use the network
or start subagents.

`--autonomous-single-turn` preserves the alternative pressure case: each agent
issues all tool calls autonomously in one model turn. Under concurrency Spawn
may legitimately batch transcript ingestion and coalesce several tool calls
into one `artifact.committed`; that mode is useful for measuring this behavior,
but the strict per-iteration timing validation fails when the checkpoints are
coalesced. Never interpret coalesced tool timing as separate physical
checkpoints. Per-turn event snapshots are merged by session sequence number so
a later 200-event tail cannot erase early iterations.

For every worker and iteration, the runner reads the JSON result and binary
artifact from the published agent branch, independently recomputes the golden
artifact hash, and checks that `golden/` and `tools/` are unchanged. It reports:

- per-iteration internal workload, observed tool-event, checkpoint-delay and
  total event-observation latency distributions (event ingestion can batch a
  fast start and completion, so these are deliberately separate measures);
- cluster wall time and stragglers for each iteration number;
- whole-run verified read bytes and new artifact bytes;
- deduplicated model token usage and transcript archive failure events;
- one-second CPU, load, memory, workspace-free-space and whole-block-device I/O
  samples (partition counters are excluded so a disk and its partitions are not
  added together);
- source size before cleanup and the number of fully verified agents.

The result directory keeps the configuration, task mapping, per-agent event
tails, host samples, every iteration's raw timing row and `cluster-report.json`.
The runner best-effort stops all known executions on success or failure and
closes its temporary mission as dropped. The golden repository and event history
remain for inspection; mounted workspaces are removed by Spawn's normal stop
path. Start with `--agents 2 --iterations 2` as a pilot before a large run.

## #97 Chaos & Recovery Baseline Matrix

In addition to throughput micro-benchmarks and cluster runs, the test suite
exercises high-concurrency contention, idempotency storms, abrupt worker
failures, workspace lifecycle races, and L1 `tmpfs` exhaustion recovery against
PostgreSQL and git borders.

### Test Coverage Matrix

| Test | Location | Concurrency / Chaos Class | Invariants Verified |
| --- | --- | --- | --- |
| `pg_concurrency_idempotency_replay_storm_claim_contention_and_event_monotonicity` | `crates/spawn-postgres/tests/recovery.rs` | 16-thread `Idempotency-Key` replay storm (`submit` + `claim` contention + `append` + `finish`) | 384 submits deduplicate to 264 unique commands; single live claim per command; idempotent duplicate `EventLog::append`; 792 strictly monotonic global events |
| `pg_worker_crash_lease_expiry_and_reconnect_takeover_zero_duplicate_side_effects` | `crates/spawn-postgres/tests/recovery.rs` | Abrupt worker connection/advisory-lock drop (`SIGKILL`) & lease TTL expiry takeover | Live worker fencing (`Rejected`); immediate takeover recovers 4 abandoned leases + 4 TTL-expired leases; `StoreError::LeaseLost` on active attempt-2 collision; zero duplicate `execution.terminal_started` events (`12/12` finished, `36` monotonic events) |
| `concurrent_workspace_lifecycle_races_git_and_d6_with_border_fsck` | `crates/spawn-host/tests/d6_layer0_cutover.rs` | 8-thread `LocalWorkspaceStore` lifecycle contention across `git` and `d6` | Sibling sentinel isolation; zero orphan dirs or `.d6-layer0-stage*` leftovers; `.gc-fence` + `reconcile_refs_journal` clean (`entries_replayed == 0`); border `git fsck --full --strict` clean |
| `d6_l1_tmpfs_budget_exhaustion_backpressure_fallback_and_recovery` | `crates/spawn-host/tests/d6_layer0_cutover.rs` | L1 `tmpfs` quota (`BudgetExceeded`), low free-space (`TmpfsFreeSpaceInsufficient`), and 80% `UsageThreshold` flush | Clean fallback to L0 disk workspace; pressure demotion reclaims `921600` bytes; automatic L1 re-admission after headroom recovery; border `git fsck --full --strict` clean |

### Reproduction Commands

Set `SPAWN_TEST_DATABASE_URL` to a disposable local PostgreSQL 16 test database
and run both integration targets:

```bash
export SPAWN_TEST_DATABASE_URL="postgres://<user>:<pass>@127.0.0.1:<port>/<db>"
cargo test --locked --offline -p spawn-postgres --test recovery -- --nocapture
cargo test --locked --offline -p spawn-host --test d6_layer0_cutover -- --nocapture
```

### Measured N252 Baseline Latency Metrics

Observed on Linux (PG 16 isolated schema) across `6/6` passing `recovery` tests
and `8/8` passing `d6_layer0_cutover` tests:

| Suite / Backend | Workload Shape | Measured Latency / Recovery Metrics |
| --- | --- | --- |
| `pg_replay_storm` | 16 threads, 384 submits -> 264 unique cmds, 792 events | `submit_ms=(52.45/333.58/405.33)` · `claim_ms=(2.57/9.26/19.59)` · `append_ms=(11.48/60.74/113.74)` · `finish_ms=(11.95/54.76/113.13)` (p50/p95/max) |
| `pg_crash_recovery` | 4 `SIGKILL` abandoned leases + 4 TTL-expired leases (`12` cmds) | `recover_ms=6.69` · `ttl_takeover_ms=107.15` |
| `ws_lifecycle_storm` (`git`) | 8 threads, 26 lifecycle cycles | `p50_ms=81.06` · `p95_ms=126.29` · `max_ms=174.75` |
| `ws_lifecycle_storm` (`d6` historical baseline before `d6-shadow` sunset) | 8 threads, 26 lifecycle cycles (border `fsck` clean) | `p50_ms=250.37` · `p95_ms=272.53` · `max_ms=274.96` |
| `d6_l1_backpressure` | L1 quota/space fallback -> demote (`921600` B) -> recover | `fallback_ms=4.07` · `demote_ms=31.21` · `recover_ms=21.16` |

