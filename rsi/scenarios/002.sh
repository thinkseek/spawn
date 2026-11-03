#!/bin/sh
# Objective 002: make ingest faster without changing what it ingests.
# rsi: ratchet
#
# Black box over the public ingest API. The bench is the controller's copy
# (assets/ingest_objective.rs), copied into both trees, so the tree under test
# cannot change how it is measured. Payloads are generated from a fresh seed.
#
# Passes when, on the same seed:
#   1. the candidate ingests the corpus to byte-identical events, and
#   2. its median total ns/op is at least 5% below the baseline's,
#      over interleaved runs (baseline, candidate, baseline, ...).
#
# On the baseline tree itself the comparison is with itself, so it fails:
# that is the red half of the gate.
#
# Prints `METRIC ingest_total_ns baseline=<n> candidate=<n>` for the record.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bench_src="$here/assets/ingest_objective.rs"
candidate=$(pwd)
baseline=${SPAWN_BASELINE_TREE:?the gate must export SPAWN_BASELINE_TREE}
rounds=${RSI_ROUNDS:-7}
seed=${RSI_SEED:-$(date +%s)}
export RSI_SEED=$seed

# The bench is built in a directory the scenario owns: the baseline tree's
# manifests, lock file and build configuration, with only the tree's
# `crates/spawn-core/src` copied in and no build script. A candidate's
# Cargo.toml, build.rs or .cargo/ therefore cannot change what is compiled
# or overwrite the bench (review finding: a build.rs could rewrite it).
build() {
    tree=$1
    name=$2
    dir="$candidate/.rsi-bench-$name"
    rm -rf "$dir"
    mkdir -p "$dir"
    (cd "$baseline" && tar -cf - --exclude=./target .) | tar -xf - -C "$dir"
    rm -rf "$dir/crates/spawn-core/src" "$dir/.cargo"
    find "$dir" -name build.rs -delete
    cp -r "$tree/crates/spawn-core/src" "$dir/crates/spawn-core/src"
    cp "$bench_src" "$dir/crates/spawn-core/examples/rsi_ingest_objective.rs"
    (cd "$dir" && CARGO_TARGET_DIR="$dir/target" cargo build -q --release \
        -p spawn-core --example rsi_ingest_objective) || return 1
    echo "$dir/target/release/examples/rsi_ingest_objective"
}

# Exactly one `total_ns <n>` line per run; anything else fails the run, so
# code under test cannot print its own number next to the bench's.
measure() {
    lines=$("$1" time 2>/dev/null | grep -E '^total_ns [0-9]+$' || true)
    [ "$(printf '%s\n' "$lines" | grep -c .)" -eq 1 ] || { echo "bench printed no single total" >&2; return 1; }
    printf '%s\n' "$lines" | awk '{print $2}'
}

base_bin=$(build "$baseline" base) || { echo "baseline bench does not build"; exit 1; }
cand_bin=$(build "$candidate" cand) || { echo "candidate bench does not build"; exit 1; }

# 1. Same events on the same corpus.
"$base_bin" check >"$candidate/.rsi-check-base" 2>/dev/null
"$cand_bin" check >"$candidate/.rsi-check-cand" 2>/dev/null
if ! cmp -s "$candidate/.rsi-check-base" "$candidate/.rsi-check-cand"; then
    echo "ingest output differs from the baseline (seed $seed):"
    diff "$candidate/.rsi-check-base" "$candidate/.rsi-check-cand" | head -20
    exit 1
fi
echo "equivalence: $(wc -l <"$candidate/.rsi-check-base") payloads identical (seed $seed)"

# 2. Interleaved timing; medians.
: >"$candidate/.rsi-base-times"
: >"$candidate/.rsi-cand-times"
i=0
while [ "$i" -lt "$rounds" ]; do
    measure "$base_bin" >>"$candidate/.rsi-base-times" || exit 1
    measure "$cand_bin" >>"$candidate/.rsi-cand-times" || exit 1
    i=$((i + 1))
done
median() { sort -n "$1" | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
base_ns=$(median "$candidate/.rsi-base-times")
cand_ns=$(median "$candidate/.rsi-cand-times")
echo "baseline runs: $(tr '\n' ' ' <"$candidate/.rsi-base-times")"
echo "candidate runs: $(tr '\n' ' ' <"$candidate/.rsi-cand-times")"
echo "METRIC ingest_total_ns baseline=$base_ns candidate=$cand_ns"

if [ "$candidate" = "$baseline" ]; then
    echo "baseline compared with itself"
    exit 1
fi
# candidate <= 95% of baseline
if [ $((cand_ns * 100)) -le $((base_ns * 95)) ]; then
    echo "improved by $(( (base_ns - cand_ns) * 100 / base_ns ))%"
    exit 0
fi
echo "not 5% faster: $cand_ns ns vs $base_ns ns"
exit 1
