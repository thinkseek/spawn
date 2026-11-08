#!/usr/bin/env bash
# Run the black-box scenarios, the e2e and the solved RSI scenarios under
# adversarial timing (scripts/timing/timing.sh), for N seeds, and summarise
# pass and fail per scenario per seed.
#
# usage: scripts/timing/run.sh [-n <seeds>] [-s "<seed> ..."] [-o <dir>] [-j <jobs>]
#                              [-k <kinds>] [-x <scale>] [<target> ...]
#
#   -n N        seeds 1..N (default 3)
#   -s LIST     these seeds instead; `off` is a run without perturbation
#   -o DIR      results (default $TMPDIR/spawn-timing-<time>): per seed and
#               target the log, the perturbation log, and summary.{tsv,md}
#   -j N        targets of one seed run N at a time (default 1)
#   -k KINDS    SPAWN_TIMING_KINDS (default all; `all,-flaky` leaves one out)
#   -x PCT      SPAWN_TIMING_SCALE (default 100)
#   target      bb:<scenario> (scripts/blackbox/scenarios/<scenario>.sh),
#               bb (all of them), e2e, rsi:<NNN> (rsi/scenarios/NNN.sh);
#               default: bb e2e rsi:001 rsi:003 rsi:004
#
# Needs SPAWN_BIN (the binary under test) and SPAWN_TEST_DATABASE_URL (the
# test server), like the gate's checks. Exit 0 when every run passed, 1
# otherwise.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
seeds=""
count=3
out=""
jobs=1
kinds=${SPAWN_TIMING_KINDS:-all}
scale=${SPAWN_TIMING_SCALE:-100}
while getopts "n:s:o:j:k:x:" option; do
    case "$option" in
    n) count=$OPTARG ;;
    s) seeds=$OPTARG ;;
    o) out=$OPTARG ;;
    j) jobs=$OPTARG ;;
    k) kinds=$OPTARG ;;
    x) scale=$OPTARG ;;
    *) sed -n '2,24p' "$0" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ -n "$seeds" ] || seeds=$(seq 1 "$count" | tr '\n' ' ')
: "${SPAWN_BIN:?SPAWN_BIN is required}"
: "${SPAWN_TEST_DATABASE_URL:?SPAWN_TEST_DATABASE_URL is required}"
[ -x "$SPAWN_BIN" ] || { echo "timing: no binary at $SPAWN_BIN" >&2; exit 2; }
SPAWN_BIN=$(cd "$(dirname "$SPAWN_BIN")" && pwd)/$(basename "$SPAWN_BIN")
out=${out:-${TMPDIR:-/tmp}/spawn-timing-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out"
out=$(cd "$out" && pwd)

targets=("$@")
[ ${#targets[@]} -gt 0 ] || targets=(bb e2e rsi:001 rsi:003 rsi:004)
expanded=()
for target in "${targets[@]}"; do
    if [ "$target" = bb ]; then
        for scenario in "$root"/scripts/blackbox/scenarios/*.sh; do
            expanded+=("bb:$(basename "$scenario" .sh)")
        done
    else
        expanded+=("$target")
    fi
done

# shellcheck source=scripts/pg-url.sh
. "$root/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" || { echo "timing: bad SPAWN_TEST_DATABASE_URL" >&2; exit 2; }
pg_base=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

# run_one <seed> <target>: one run, its log in $out/<seed>/<target>.log and
# its result line in $out/<seed>/<target>.result: "<rc> <seconds>".
run_one() {
    local seed=$1 target=$2 name=${2//:/-}
    local dir=$out/$seed log=$out/$seed/$name.log
    local started=$SECONDS rc
    mkdir -p "$dir"
    (
        if [ "$seed" = off ]; then
            unset SPAWN_TIMING SPAWN_TIMING_SEED
        else
            export SPAWN_TIMING=adversarial SPAWN_TIMING_SEED=$seed
            export SPAWN_TIMING_KINDS=$kinds SPAWN_TIMING_SCALE=$scale
            export SPAWN_TIMING_OUT=$dir/$name.timing
        fi
        case "$target" in
        bb:*)
            SPAWN_PSQL=psql SPAWN_PG_BASE=$pg_base \
                exec bash "$root/scripts/blackbox/run-scenarios.sh" "${target#bb:}.sh" ;;
        e2e)
            SPAWN_DATABASE_URL=$SPAWN_TEST_DATABASE_URL exec bash "$root/scripts/e2e.sh" ;;
        rsi:*)
            if [ "$seed" = off ]; then
                exec sh "$root/rsi/scenarios/${target#rsi:}.sh"
            fi
            exec bash "$root/scripts/timing/with.sh" -d "$SPAWN_TIMING_OUT/timing-rsi" \
                sh "$root/rsi/scenarios/${target#rsi:}.sh" ;;
        *) echo "timing: unknown target $target" >&2; exit 2 ;;
        esac
    ) >"$log" 2>&1 </dev/null
    rc=$?
    echo "$rc $((SECONDS - started))" >"$dir/$name.result"
    printf 'timing: seed %-4s %-22s %s (%ss)\n' "$seed" "$target" \
        "$([ "$rc" -eq 0 ] && echo pass || echo "FAIL rc=$rc")" "$((SECONDS - started))"
}

echo "timing: seeds: $seeds; kinds: $kinds; scale: $scale%; binary: $SPAWN_BIN; results: $out"
for seed in $seeds; do
    running=0
    for target in "${expanded[@]}"; do
        run_one "$seed" "$target" &
        running=$((running + 1))
        if [ "$running" -ge "$jobs" ]; then
            wait -n
            running=$((running - 1))
        fi
    done
    wait
    # Mirror daemons outlive nothing: their scenario's directory is gone.
    pkill -f "python3 -I $out/$seed/.*/mirror.py" 2>/dev/null || true
done

# The table: one row per target, one column per seed.
{
    printf 'target'
    for seed in $seeds; do printf '\t%s' "$seed"; done
    printf '\n'
    for target in "${expanded[@]}"; do
        printf '%s' "$target"
        for seed in $seeds; do
            read -r rc _ <"$out/$seed/${target//:/-}.result" 2>/dev/null || rc=x
            case "$rc" in 0) cell=pass ;; x) cell=- ;; *) cell="FAIL($rc)" ;; esac
            printf '\t%s' "$cell"
        done
        printf '\n'
    done
} >"$out/summary.tsv"
{
    head -1 "$out/summary.tsv" | sed 's/\t/ | /g; s/^/| /; s/$/ |/'
    head -1 "$out/summary.tsv" | sed 's/[^\t]*/---/g; s/\t/ | /g; s/^/| /; s/$/ |/'
    tail -n +2 "$out/summary.tsv" | sed 's/\t/ | /g; s/^/| /; s/$/ |/'
} >"$out/summary.md"
cat "$out/summary.md"
echo "timing: results in $out"
! grep -q 'FAIL' "$out/summary.tsv"
