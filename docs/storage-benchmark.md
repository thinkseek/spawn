# Storage benchmark

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

## Run

```sh
cargo build --release -p spawn-host --example storage_benchmark

target/release/examples/storage_benchmark \
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

## Compare repeated runs

Use at least three independent runs when judging an optimization. Do not compare
one baseline run with one candidate run and do not mix configurations.

```sh
python3 -I scripts/storage-benchmark-compare.py \
  --baseline baseline-1.json baseline-2.json baseline-3.json \
  --candidate candidate-1.json candidate-2.json candidate-3.json
```

The comparison uses the median across runs of each operation's p50 and p95. It
also compares whole-cycle throughput and exits 1 on a median-p50 or throughput
regression over 10% (`--max-regression-percent` changes that bound). Latency
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
