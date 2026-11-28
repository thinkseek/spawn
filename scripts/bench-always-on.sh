#!/usr/bin/env bash
# End-to-end Always-On production readiness benchmark orchestrator & Go/No-Go Scorecard.
#
# Drives:
#   Dimension A: D6 (L1 tmpfs + FastCDC + CAS Merkle) at 8 & 16 concurrent branches
#   Dimension B: Isolated 1 CPU / 1 GiB RAM PG16 (max_connections=30) concurrency & sharding
#   Dimension C: Context & work-hours lineage completeness verification
#   Dimension D: Deterministic Always-On Go/No-Go Scorecard (JSON + Markdown)
set -euo pipefail

usage() {
    cat <<'EOF'
Usage: scripts/bench-always-on.sh [options]

  --out-dir <dir>         Output directory for scorecard.{json,md} & sub-reports
  --pg-mode <mode>        auto (default), docker, systemd, or local
  --d6-iterations <n>     Measured D6 checkpoint cycles per branch (default: 12)
  --d6-warmup <n>         Warm-up D6 cycles per branch (default: 2)
  --d6-files <n>          Workspace files per branch (default: 128)
  --d6-file-bytes <n>     Bytes per workspace file (default: 4096)
  --fastcdc-bytes <n>     Oversized FastCDC file bytes > 4 MiB (default: 5242880)
  --pg-pool-size <n>      Shared PG pool size for 16-branch D6 run (default: 12)
  --skip-build            Reuse already compiled release benchmark binaries
  --help, -h              Show this help
EOF
}

OUT_DIR=""
PG_MODE="auto"
D6_ITERATIONS=12
D6_WARMUP=2
D6_FILES=128
D6_FILE_BYTES=4096
FASTCDC_BYTES=5242880
PG_POOL_SIZE=12
SKIP_BUILD=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --out-dir) OUT_DIR="$2"; shift 2 ;;
        --pg-mode) PG_MODE="$2"; shift 2 ;;
        --d6-iterations) D6_ITERATIONS="$2"; shift 2 ;;
        --d6-warmup) D6_WARMUP="$2"; shift 2 ;;
        --d6-files) D6_FILES="$2"; shift 2 ;;
        --d6-file-bytes) D6_FILE_BYTES="$2"; shift 2 ;;
        --fastcdc-bytes) FASTCDC_BYTES="$2"; shift 2 ;;
        --pg-pool-size) PG_POOL_SIZE="$2"; shift 2 ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --help|-h) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

if [ -f /data00/spawn-toolchain/env ]; then
    # shellcheck disable=SC1091
    source /data00/spawn-toolchain/env
fi
if [ -d /data00/spawn-toolchain/bin-m2-pg ]; then
    export PATH="/data00/spawn-toolchain/bin-m2-pg:$PATH"
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "bench-always-on: python3 is required" >&2
    exit 2
fi

if [ "$PG_MODE" = "auto" ]; then
    if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
        PG_MODE="docker"
    elif command -v systemd-run >/dev/null 2>&1 && systemd-run --user --scope true >/dev/null 2>&1; then
        PG_MODE="systemd"
    else
        PG_MODE="local"
    fi
fi

if [ -z "$OUT_DIR" ]; then
    OUT_DIR="$(mktemp -d "${TMPDIR:-/tmp}/spawn-always-on-scorecard.XXXXXX")"
else
    mkdir -p "$OUT_DIR"
fi

PG_TMP_BASE="${TMPDIR:-/tmp}"
# Keep Unix domain socket path well below Linux 107-byte SUN_LEN limit.
if [ "${#PG_TMP_BASE}" -gt 50 ]; then
    PG_TMP_BASE="/tmp"
fi
WORK_TMP="$(mktemp -d "$PG_TMP_BASE/spawn-always-on-pg.XXXXXX")"
TMPFS_BASE="/tmp"
if [ -d /dev/shm ] && [ -w /dev/shm ]; then
    TMPFS_BASE="/dev/shm"
fi
TMPFS_8="$TMPFS_BASE/spawn-bench-l1-8-$$"
TMPFS_16="$TMPFS_BASE/spawn-bench-l1-16-$$"
PG_DOCKER_NAME="spawn-always-on-pg-$$"
PG_PID=""
MON_PID=""
PG_SCOPE_UNIT=""
BENCH_URL=""
PSQL_TARGET=""

cleanup() {
    set +e
    if [ -n "$MON_PID" ]; then
        kill "$MON_PID" 2>/dev/null || true
        wait "$MON_PID" 2>/dev/null || true
    fi
    if [ "$PG_MODE" != "docker" ] && [ -f "$WORK_TMP/pgdata/postmaster.pid" ] && command -v pg_ctl >/dev/null 2>&1; then
        pg_ctl -D "$WORK_TMP/pgdata" -m fast stop >/dev/null 2>&1 \
            || pg_ctl -D "$WORK_TMP/pgdata" -m immediate stop >/dev/null 2>&1 \
            || true
    fi
    if [ -n "$PG_PID" ]; then
        kill -TERM "$PG_PID" 2>/dev/null || true
        for _ in $(seq 1 30); do
            kill -0 "$PG_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$PG_PID" 2>/dev/null || true
        wait "$PG_PID" 2>/dev/null || true
    fi
    if [ "$PG_MODE" = "docker" ]; then
        docker rm -f -v "$PG_DOCKER_NAME" >/dev/null 2>&1 || true
    fi
    if [ -n "$PG_SCOPE_UNIT" ] && command -v systemctl >/dev/null 2>&1; then
        systemctl --user stop "$PG_SCOPE_UNIT" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK_TMP" "$TMPFS_8" "$TMPFS_16" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

if [ "$SKIP_BUILD" -eq 0 ]; then
    cargo build --offline --release -p spawn-host \
        --example storage_benchmark \
        --example always_on_control_plane_bench
fi

STORAGE_BIN="$REPO_ROOT/target/release/examples/storage_benchmark"
CP_BIN="$REPO_ROOT/target/release/examples/always_on_control_plane_bench"
if [ ! -x "$STORAGE_BIN" ] || [ ! -x "$CP_BIN" ]; then
    echo "bench-always-on: missing release benchmark binaries under target/release/examples" >&2
    exit 1
fi

PG_FLAGS=(
    -c "max_connections=30"
    -c "shared_buffers=128MB"
    -c "work_mem=4MB"
    -c "maintenance_work_mem=32MB"
    -c "effective_cache_size=512MB"
    -c "fsync=on"
    -c "synchronous_commit=on"
    -c "log_lock_waits=on"
)

start_postgres() {
    case "$PG_MODE" in
        docker)
            local port
            port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
            docker run -d \
                --name "$PG_DOCKER_NAME" \
                --cpus=1 \
                --memory=1g \
                --memory-swap=1g \
                -p "127.0.0.1:${port}:5432" \
                -e POSTGRES_HOST_AUTH_METHOD=trust \
                -e POSTGRES_USER=postgres \
                -e POSTGRES_DB=spawn_bench \
                postgres:16 \
                "${PG_FLAGS[@]}" >/dev/null
            BENCH_URL="postgres://postgres@127.0.0.1:${port}/spawn_bench"
            for _ in $(seq 1 60); do
                if docker exec "$PG_DOCKER_NAME" pg_isready -U postgres -d spawn_bench >/dev/null 2>&1; then
                    break
                fi
                sleep 0.2
            done
            PSQL_TARGET="$BENCH_URL"
            ;;
        systemd|local)
            local pgdata="$WORK_TMP/pgdata"
            local pgsock="$WORK_TMP/pgsock"
            local pglog="$WORK_TMP/pg.log"
            mkdir -p "$pgsock"
            chmod 700 "$pgsock"
            initdb -D "$pgdata" -U postgres -A trust --encoding=UTF8 --locale=C >/dev/null
            if [ "$PG_MODE" = "systemd" ]; then
                PG_SCOPE_UNIT="spawn-bench-pg16-$$.scope"
                systemd-run --user --scope --quiet --unit="$PG_SCOPE_UNIT" \
                    -p "CPUQuota=100%" \
                    -p "MemoryMax=1G" \
                    -p "MemorySwapMax=0" \
                    postgres -D "$pgdata" -h "" -k "$pgsock" "${PG_FLAGS[@]}" \
                    >"$pglog" 2>&1 &
                PG_PID=$!
            else
                taskset -c 0 postgres -D "$pgdata" -h "" -k "$pgsock" "${PG_FLAGS[@]}" \
                    >"$pglog" 2>&1 &
                PG_PID=$!
            fi
            for _ in $(seq 1 60); do
                if pg_isready -h "$pgsock" -U postgres >/dev/null 2>&1; then
                    break
                fi
                sleep 0.2
            done
            createdb -h "$pgsock" -U postgres spawn_bench
            BENCH_URL="postgres://postgres@localhost/spawn_bench?host=${pgsock}"
            PSQL_TARGET="-h $pgsock -U postgres -d spawn_bench"
            ;;
        *)
            echo "Invalid --pg-mode: $PG_MODE" >&2
            exit 2
            ;;
    esac
}

run_psql_scalar() {
    local sql="$1"
    if [ "$PG_MODE" = "docker" ]; then
        docker exec "$PG_DOCKER_NAME" psql -U postgres -d spawn_bench -Atqc "$sql" 2>/dev/null || echo ""
    else
        # shellcheck disable=SC2086
        psql $PSQL_TARGET -Atqc "$sql" 2>/dev/null || echo ""
    fi
}

monitor_pg_loop() {
    local mon_log="$WORK_TMP/pg_samples.jsonl"
    : >"$mon_log"
    while true; do
        local stats=""
        stats="$(run_psql_scalar "select coalesce(count(*),0)::text || ',' || coalesce(count(*) filter (where state = 'active'),0)::text || ',' || coalesce((select sum(deadlocks) from pg_stat_database where datname='spawn_bench'),0)::text || ',' || coalesce((select pid from pg_stat_activity where pid = pg_backend_pid() limit 1),0)::text from pg_stat_activity where datname='spawn_bench';")"
        local rss_kb="0"
        if [ "$PG_MODE" = "docker" ]; then
            rss_kb="$(docker stats --no-stream --format '{{.MemUsage}}' "$PG_DOCKER_NAME" 2>/dev/null | python3 -c '
import re, sys
s = sys.stdin.read().strip().split("/")[0].strip()
m = re.match(r"([0-9.]+)\s*([KMGT]?i?B)", s)
if not m:
    print(0)
else:
    v, u = float(m.group(1)), m.group(2)
    mult = {"B": 1/1024, "KiB": 1, "KB": 1000/1024, "MiB": 1024, "MB": 1000, "GiB": 1024*1024, "GB": 1000*1000}.get(u, 0)
    print(int(v * mult))
')"
        elif [ -d "$WORK_TMP/pgdata" ]; then
            rss_kb="$(python3 - "$WORK_TMP/pgdata" <<'PY'
import glob, os, sys
pgdata = os.path.realpath(sys.argv[1])
pids = set()
pm_pid = os.path.join(pgdata, "postmaster.pid")
try:
    with open(pm_pid) as f:
        root_pid = int(f.readline().strip())
    pids.add(root_pid)
except Exception:
    pass
if pids:
    children = {}
    for st in glob.glob("/proc/[0-9]*/stat"):
        try:
            parts = open(st).read().split()
            pid = int(parts[0])
            ppid = int(parts[3])
            children.setdefault(ppid, []).append(pid)
        except Exception:
            pass
    stack = list(pids)
    while stack:
        cur = stack.pop()
        for ch in children.get(cur, []):
            if ch not in pids:
                pids.add(ch)
                stack.append(ch)
rss = 0
for pid in pids:
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                rss += int(line.split()[1])
                break
    except Exception:
        pass
print(rss)
PY
)"
        fi
        if [ -n "$stats" ]; then
            IFS=',' read -r total_conn active_conn deadlocks_total _unused <<<"$stats"
            printf '{"total_conn":%s,"active_conn":%s,"deadlocks_total":%s,"rss_kb":%s}\n' \
                "${total_conn:-0}" "${active_conn:-0}" "${deadlocks_total:-0}" "${rss_kb:-0}" >>"$mon_log"
        fi
        sleep 0.5
    done
}

start_postgres
monitor_pg_loop &
MON_PID=$!

SOURCE_REV="$(git rev-parse HEAD)"

echo "[1/3] Running Dimension A (8-branch D6 L1+FastCDC+CrashDrill)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$STORAGE_BIN" \
    --backend d6 \
    --concurrency 8 \
    --pg-pool-size 8 \
    --tmpfs-root "$TMPFS_8" \
    --tmpfs-budget-mb 2048 \
    --iterations "$D6_ITERATIONS" \
    --warmup "$D6_WARMUP" \
    --files "$D6_FILES" \
    --file-bytes "$D6_FILE_BYTES" \
    --edits 4 \
    --oversized-file-bytes "$FASTCDC_BYTES" \
    --land-branches \
    --crash-restart-drill \
    --source-revision "$SOURCE_REV" \
    >"$OUT_DIR/d6_8.json"

echo "[2/3] Running Dimension A (16-branch D6 L1+FastCDC+CrashDrill)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$STORAGE_BIN" \
    --backend d6 \
    --concurrency 16 \
    --pg-pool-size "$PG_POOL_SIZE" \
    --tmpfs-root "$TMPFS_16" \
    --tmpfs-budget-mb 2048 \
    --iterations "$D6_ITERATIONS" \
    --warmup "$D6_WARMUP" \
    --files "$D6_FILES" \
    --file-bytes "$D6_FILE_BYTES" \
    --edits 4 \
    --oversized-file-bytes "$FASTCDC_BYTES" \
    --land-branches \
    --crash-restart-drill \
    --source-revision "$SOURCE_REV" \
    >"$OUT_DIR/d6_16.json"

echo "[3/3] Running Dimensions B & C (1C/1G PG16 + Sharded Claim + Attribution)..." >&2
SPAWN_BENCH_DATABASE_URL="$BENCH_URL" "$CP_BIN" \
    --sessions 16 \
    --events-per-session 40 \
    --shards 4 \
    --commands-per-session 16 \
    --writer-pool 8 \
    --attribution-tasks 16 \
    >"$OUT_DIR/control_plane.json"

kill "$MON_PID" 2>/dev/null || true
wait "$MON_PID" 2>/dev/null || true
MON_PID=""

PG_VER="$(run_psql_scalar "show server_version;" || echo "16")"
PG_MAX_CONN="$(run_psql_scalar "show max_connections;" || echo "30")"
UNAME_ARCH="$(uname -m)"
CPU_CORES="$(nproc 2>/dev/null || echo 1)"

python3 - "$OUT_DIR" "$WORK_TMP/pg_samples.jsonl" "$PG_MODE" "$PG_VER" "$PG_MAX_CONN" "$SOURCE_REV" "$UNAME_ARCH" "$CPU_CORES" <<'PY'
import datetime, json, pathlib, sys

out_dir = pathlib.Path(sys.argv[1])
samples_path = pathlib.Path(sys.argv[2])
pg_mode, pg_ver, pg_max_conn, source_rev, arch, cpu_cores = sys.argv[3:9]

d6_8 = json.loads((out_dir / "d6_8.json").read_text())
d6_16 = json.loads((out_dir / "d6_16.json").read_text())
cp = json.loads((out_dir / "control_plane.json").read_text())

samples = []
if samples_path.exists():
    for line in samples_path.read_text().splitlines():
        line = line.strip()
        if line:
            samples.append(json.loads(line))

peak_total_conn = max((s["total_conn"] for s in samples), default=0)
peak_active_conn = max((s["active_conn"] for s in samples), default=0)
peak_rss_kb = max((s["rss_kb"] for s in samples), default=0)
peak_rss_mib = round(peak_rss_kb / 1024.0, 2)
pg_deadlocks_delta = (samples[-1]["deadlocks_total"] - samples[0]["deadlocks_total"]) if len(samples) >= 2 else (samples[0]["deadlocks_total"] if samples else 0)

ao_8 = d6_8["always_on_d6"]
ao_16 = d6_16["always_on_d6"]
ev = cp["dimension_b_pg16"]["event_log_xid8"]
sh = cp["dimension_b_pg16"]["sharded_bridge_claim"]
attr = cp["dimension_c_attribution"]

gates = [
    {
        "id": "D6_8_LAND_DELTA",
        "dimension": "A: D6 8-Branch",
        "metric": "max_bytes_per_landing",
        "measured": f"{ao_8['max_bytes_per_landing']:,} B ({ao_8['max_bytes_per_landing'] / (1024*1024):.3f} MiB)",
        "threshold": "< 52,428,800 B (50.0 MiB)",
        "passed": ao_8["max_bytes_per_landing"] < 52_428_800,
    },
    {
        "id": "D6_16_LAND_DELTA",
        "dimension": "A: D6 16-Branch",
        "metric": "max_bytes_per_landing",
        "measured": f"{ao_16['max_bytes_per_landing']:,} B ({ao_16['max_bytes_per_landing'] / (1024*1024):.3f} MiB)",
        "threshold": "< 52,428,800 B (50.0 MiB)",
        "passed": ao_16["max_bytes_per_landing"] < 52_428_800,
    },
    {
        "id": "D6_8_DEDUP_RATIO",
        "dimension": "A: D6 8-Branch",
        "metric": "dedup_ratio",
        "measured": f"{ao_8['dedup_ratio']:.2f}x",
        "threshold": ">= 2.00x",
        "passed": ao_8["dedup_ratio"] >= 2.0,
    },
    {
        "id": "D6_16_DEDUP_RATIO",
        "dimension": "A: D6 16-Branch",
        "metric": "dedup_ratio",
        "measured": f"{ao_16['dedup_ratio']:.2f}x",
        "threshold": ">= 2.00x",
        "passed": ao_16["dedup_ratio"] >= 2.0,
    },
    {
        "id": "D6_8_CHECKPOINT_P95",
        "dimension": "A: D6 8-Branch",
        "metric": "checkpoint_changed p95_ms",
        "measured": f"{d6_8['operations']['checkpoint_changed']['p95_ms']:.2f} ms",
        "threshold": "<= 500.00 ms",
        "passed": d6_8["operations"]["checkpoint_changed"]["p95_ms"] <= 500.0,
    },
    {
        "id": "D6_16_CHECKPOINT_P95",
        "dimension": "A: D6 16-Branch",
        "metric": "checkpoint_changed p95_ms",
        "measured": f"{d6_16['operations']['checkpoint_changed']['p95_ms']:.2f} ms",
        "threshold": "<= 1000.00 ms",
        "passed": d6_16["operations"]["checkpoint_changed"]["p95_ms"] <= 1000.0,
    },
    {
        "id": "D6_16_LAND_P95",
        "dimension": "A: D6 16-Branch",
        "metric": "land_main p95_ms",
        "measured": f"{d6_16['operations']['land_main']['p95_ms']:.2f} ms",
        "threshold": "<= 500.00 ms",
        "passed": d6_16["operations"]["land_main"]["p95_ms"] <= 500.0,
    },
    {
        "id": "D6_CRASH_RESTART_ZERO_LOSS",
        "dimension": "A: Crash-Restart",
        "metric": "lost_refs / lost_commits / corrupt_objects",
        "measured": (
            f"8b=({ao_8['crash_restart_drill']['lost_refs']}/{ao_8['crash_restart_drill']['lost_commits']}/{ao_8['crash_restart_drill']['corrupt_objects']}), "
            f"16b=({ao_16['crash_restart_drill']['lost_refs']}/{ao_16['crash_restart_drill']['lost_commits']}/{ao_16['crash_restart_drill']['corrupt_objects']})"
        ),
        "threshold": "0 / 0 / 0 across 8 & 16 branches",
        "passed": bool(ao_8["crash_restart_drill"]["verified"] and ao_16["crash_restart_drill"]["verified"]),
    },
    {
        "id": "PG16_XID8_WATERMARK_INTEGRITY",
        "dimension": "B: 1C/1G PG16",
        "metric": "session OOO / dup / miss / deadlock",
        "measured": f"{ev['out_of_order_reads']} / {ev['duplicate_reads']} / {ev['missing_reads']} / {ev['deadlocks']} ({ev['events_written']}/{ev['events_target']} events, xid8_reorders={ev.get('commit_id_inversions', 0)})",
        "threshold": "0 / 0 / 0 / 0 and write_errors=0",
        "passed": (
            ev["events_written"] == ev["events_target"]
            and ev["out_of_order_reads"] == 0
            and ev["duplicate_reads"] == 0
            and ev["missing_reads"] == 0
            and ev.get("unexpected_reads", 0) == 0
            and ev["deadlocks"] == 0
            and ev["write_errors"] == 0
            and ev["read_errors"] == 0
        ),
    },
    {
        "id": "PG16_SHARD_CLAIM_INTEGRITY",
        "dimension": "B: 1C/1G PG16",
        "metric": "dup / unclaimed / wrong_shard / deadlock",
        "measured": f"{sh['duplicate_claims']} / {sh['unclaimed_commands']} / {sh['wrong_shard_claims']} / {sh['deadlocks']} ({sh['commands_claimed_unique']}/{sh['commands_target']})",
        "threshold": "0 / 0 / 0 / 0 and full claim",
        "passed": (
            sh["commands_claimed_unique"] == sh["commands_target"]
            and sh["duplicate_claims"] == 0
            and sh["unclaimed_commands"] == 0
            and sh["wrong_shard_claims"] == 0
            and sh["deadlocks"] == 0
            and sh["submit_errors"] == 0
            and sh["claim_errors"] == 0
            and sh["finish_errors"] == 0
        ),
    },
    {
        "id": "PG16_RESOURCE_ENVELOPE",
        "dimension": "B: 1C/1G PG16",
        "metric": "peak_conn / peak_rss_mib / pg_deadlocks",
        "measured": f"{peak_total_conn}/{pg_max_conn} conns, {peak_rss_mib:.2f} MiB RSS, {pg_deadlocks_delta} deadlocks",
        "threshold": "<= 27 conns, < 900.0 MiB RSS, 0 deadlocks",
        "passed": peak_total_conn <= 27 and peak_rss_mib < 900.0 and pg_deadlocks_delta == 0,
    },
    {
        "id": "ATTRIBUTION_COMPLETENESS",
        "dimension": "C: Attribution",
        "metric": "completeness_pct & field_gaps",
        "measured": f"{attr['completeness_pct']:.2f}% ({attr['tasks_fully_attributed']}/{attr['tasks_total']}), gaps={attr['attribution_gaps']}, dur_ms={attr['total_work_duration_ms']}, usd={attr['total_cost_usd']:.4f}",
        "threshold": "100.00%, gaps=0, unpriced=0",
        "passed": (
            attr["tasks_fully_attributed"] == attr["tasks_total"]
            and attr["attribution_gaps"] == 0
            and attr["unpriced_tokens"] == 0
            and attr["total_cost_usd"] > 0.0
            and attr["total_work_duration_ms"] > 0
        ),
    },
]

all_passed = all(g["passed"] for g in gates)
verdict = "GO" if all_passed else "NO-GO"

scorecard = {
    "schema_version": 1,
    "generated_at_utc": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "verdict": verdict,
    "gates_passed": sum(1 for g in gates if g["passed"]),
    "gates_total": len(gates),
    "environment": {
        "source_revision": source_rev,
        "arch": arch,
        "host_cpu_cores": int(cpu_cores),
        "pg_mode": pg_mode,
        "pg_version": pg_ver,
        "pg_max_connections": int(pg_max_conn),
        "pg_envelope": "1 CPU / 1 GiB RAM",
    },
    "pg_monitor": {
        "samples": len(samples),
        "peak_total_connections": peak_total_conn,
        "peak_active_connections": peak_active_conn,
        "peak_pg_rss_mib": peak_rss_mib,
        "pg_stat_deadlocks_delta": pg_deadlocks_delta,
    },
    "gates": gates,
    "dimensions": {
        "dimension_a_d6_8_branches": d6_8,
        "dimension_a_d6_16_branches": d6_16,
        "dimension_b_and_c_control_plane": cp,
    },
}

(out_dir / "scorecard.json").write_text(json.dumps(scorecard, indent=2, sort_keys=True) + "\n")

def fmt_op(ops, key):
    op = ops.get(key)
    if not op:
        return "n/a"
    return f"p50={op['p50_ms']:.2f}ms / p95={op['p95_ms']:.2f}ms / p99={op['p99_ms']:.2f}ms (n={op['n']})"

lines = [
    f"# Always-On Go/No-Go Scorecard — Verdict: **{verdict}** ({scorecard['gates_passed']}/{scorecard['gates_total']} gates passed)",
    "",
    f"- **Generated (UTC)**: `{scorecard['generated_at_utc']}`",
    f"- **Source Revision**: `{source_rev}`",
    f"- **Host / PG16 Envelope**: `{arch}` ({cpu_cores} cores host), PG `{pg_ver}` in `{pg_mode}` 1C/1G mode (`max_connections={pg_max_conn}`)",
    f"- **PG16 Monitor**: peak `{peak_total_conn}/{pg_max_conn}` connections (active peak `{peak_active_conn}`), RSS peak `{peak_rss_mib:.2f} MiB`, PG deadlocks delta `{pg_deadlocks_delta}`",
    "",
    "## Gate Matrix (Red-Line SLOs vs Measured)",
    "",
    "| Status | Gate | Dimension | Metric | Red-Line SLO | Measured |",
    "| :--- | :--- | :--- | :--- | :--- | :--- |",
]
for g in gates:
    status = "PASS" if g["passed"] else "FAIL"
    lines.append(
        f"| **{status}** | `{g['id']}` | {g['dimension']} | `{g['metric']}` | {g['threshold']} | {g['measured']} |"
    )

lines.extend([
    "",
    "## Dimension Details",
    "",
    "### Dimension A · D6 (L1 tmpfs + FastCDC + CAS Merkle)",
    f"- **8 concurrent branches**: checkpoint={fmt_op(d6_8['operations'], 'checkpoint_changed')}; merge={fmt_op(d6_8['operations'], 'merge_base')}; land={fmt_op(d6_8['operations'], 'land_main')}; dedup=`{ao_8['dedup_ratio']:.2f}x`; mean/max land delta=`{ao_8['bytes_per_landing']:.0f}`/`{ao_8['max_bytes_per_landing']}` B; crash-restart recovery=`{ao_8['crash_restart_drill']['recovery_ms']:.2f} ms` (0 loss across `{ao_8['crash_restart_drill']['snapshots_verified']}` snapshots / `{ao_8['crash_restart_drill']['blobs_verified']}` blobs)",
    f"- **16 concurrent branches**: checkpoint={fmt_op(d6_16['operations'], 'checkpoint_changed')}; merge={fmt_op(d6_16['operations'], 'merge_base')}; land={fmt_op(d6_16['operations'], 'land_main')}; dedup=`{ao_16['dedup_ratio']:.2f}x`; mean/max land delta=`{ao_16['bytes_per_landing']:.0f}`/`{ao_16['max_bytes_per_landing']}` B; crash-restart recovery=`{ao_16['crash_restart_drill']['recovery_ms']:.2f} ms` (0 loss across `{ao_16['crash_restart_drill']['snapshots_verified']}` snapshots / `{ao_16['crash_restart_drill']['blobs_verified']}` blobs)",
    "",
    "### Dimension B · Control Plane 1C/1G PG16 Concurrency & Sharding",
    f"- **`xid8` Watermark Stress**: `{ev['events_written']}/{ev['events_target']}` events across `{ev['sessions']}` sessions / `{ev['writer_pool']}` writers at `{ev['events_per_sec']:.1f} events/s`; append={fmt_op({'a': ev['append_latency']}, 'a')}; read_global={fmt_op({'r': ev['read_global_latency']}, 'r')}",
    f"- **4-Shard Bridge Claim**: `{sh['commands_claimed_unique']}/{sh['commands_target']}` commands claimed at `{sh['claims_per_sec']:.1f} claims/s`; claim={fmt_op({'c': sh['claim_latency']}, 'c')}; finish={fmt_op({'f': sh['finish_latency']}, 'f')}; per-shard=`{json.dumps(sh['per_shard_counts'], sort_keys=True)}`",
    "",
    "### Dimension C · Context & Work-Hours Attribution Completeness",
    f"- **Attribution**: `{attr['tasks_fully_attributed']}/{attr['tasks_total']}` tasks (`{attr['completeness_pct']:.2f}%`), field gaps=`{attr['attribution_gaps']}`, work duration total=`{attr['total_work_duration_ms']} ms`, tokens=`{attr['total_tokens']}` (unpriced=`{attr['unpriced_tokens']}`), cost=`${attr['total_cost_usd']:.4f}`",
    "",
])

md = "\n".join(lines)
(out_dir / "scorecard.md").write_text(md)
print(md)
sys.exit(0 if all_passed else 1)
PY
