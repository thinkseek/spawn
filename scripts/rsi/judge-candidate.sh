#!/bin/sh
# Black-box gate for one RSI candidate.
#
# usage: judge-candidate.sh <bare-repo> <baseline-commit> <candidate-commit> <scenario>
#
# Exports both commits to temporary directories, builds the `spawn` binary in
# each, and runs the scenario. The candidate passes only when the scenario
# fails on the baseline and passes on the candidate.
#
# The scenario comes from the controller's checkout, not from the trees, so a
# candidate cannot weaken the test that judges it.
#
# Before the verdict it prints the scenario output, each line prefixed with
# `| `, so the gate log records what the scenario measured.
#
# Also enforced (docs/rsi.md §4):
# - the candidate may not touch `rsi/` or `scripts/rsi/`: the loop's tasks,
#   scenarios and gate belong to the controller;
# - every other scenario in the controller's directory must still pass on the
#   candidate, except ratchet scenarios (a line `# rsi: ratchet`), which
#   compare against the baseline and so fail on any tree that did not improve.
#
# Prints one JSON line, last.
#
# ponytail: the gate runs on the host. Move it into a container without the
# `spawn` binary when a candidate is treated as untrusted.

set -eu

repo=$1
baseline=$2
candidate=$3
scenario=$4

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The controller resolves this path against its own checkout. Refuse a
# relative path: it would resolve inside the tree under test.
case "$scenario" in
/*) ;;
*)
    echo '{"verdict":"rejected","reason":"scenario path is not absolute"}'
    exit 0
    ;;
esac
if [ ! -f "$scenario" ]; then
    echo '{"verdict":"rejected","reason":"scenario is missing"}'
    exit 0
fi

extract() {
    mkdir -p "$2"
    git --git-dir="$repo" archive "$1" | tar -x -C "$2"
}

# 0 = scenario passed, 1 = scenario failed, 2 = tree did not build.
#
# The target directory is pinned per tree. An inherited `CARGO_TARGET_DIR`
# would send the binary somewhere else, and both trees would share one output
# directory, so the candidate build could overwrite the baseline binary the
# scenario is about to run.
try() {
    tree=$1
    if ! (cd "$tree" && CARGO_TARGET_DIR="$tree/target" cargo build -q --bin spawn) \
        >"$work/build.log" 2>&1; then
        return 2
    fi
    run_scenario "$tree" "$scenario"
}

# Run one scenario in a built tree, echoing its output prefixed with `| `.
run_scenario() {
    status=0
    (cd "$1" && SPAWN_BIN="$1/target/debug/spawn" SPAWN_BASELINE_TREE="$work/baseline" \
        sh "$2") >"$work/run.log" 2>&1 || status=$?
    sed 's/^/| /' "$work/run.log" | tail -n 60
    return "$status"
}

extract "$baseline" "$work/baseline"
extract "$candidate" "$work/candidate"

touched=$(git --git-dir="$repo" diff --name-only "$baseline" "$candidate" -- rsi scripts/rsi)
if [ -n "$touched" ]; then
    echo "| candidate touches controller files: $(echo "$touched" | tr '\n' ' ')"
    echo '{"verdict":"rejected","reason":"candidate touches rsi/ or scripts/rsi/"}'
    exit 0
fi

echo "| == baseline: $(basename "$scenario") must fail"
red=0
try "$work/baseline" || red=$?
if [ "$red" -eq 2 ]; then
    echo '{"verdict":"rejected","reason":"baseline does not build"}'
    exit 0
fi
if [ "$red" -eq 0 ]; then
    echo '{"verdict":"rejected","reason":"scenario already passes on the baseline"}'
    exit 0
fi

echo "| == candidate: $(basename "$scenario") must pass"
green=0
try "$work/candidate" || green=$?
if [ "$green" -eq 2 ]; then
    echo '{"verdict":"rejected","reason":"candidate does not build"}'
    exit 0
fi
if [ "$green" -ne 0 ]; then
    echo '{"verdict":"rejected","reason":"scenario fails on the candidate"}'
    exit 0
fi

for other in "$(dirname "$scenario")"/*.sh; do
    [ "$other" = "$scenario" ] && continue
    grep -q '^# rsi: ratchet' "$other" && continue
    echo "| == regression: $(basename "$other") must still pass"
    if ! run_scenario "$work/candidate" "$other"; then
        echo "{\"verdict\":\"rejected\",\"reason\":\"regression: $(basename "$other") fails on the candidate\"}"
        exit 0
    fi
done
echo '{"verdict":"accepted"}'
