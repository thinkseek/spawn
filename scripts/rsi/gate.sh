#!/bin/sh
# The gate: one black-box judge for every landing, RSI or fleet
# (docs/fleet-architecture.md §10.2).
#
# usage: gate.sh <bare-repo> <base-commit> <candidate-commit> <scenarios-dir> [<scenario>]
#
# Exports both commits to temporary directories and judges the candidate
# with the controller's scenarios, never the trees' own: a candidate cannot
# weaken the test that judges it.
#
# - The candidate may not change a path listed in `<scenarios-dir>/protected`
#   (one prefix per line, `#` comments): the controller's tasks, scenarios,
#   gate and black-box tests.
# - It may not add build-time code (`build.rs`, `.cargo/`): that would run on
#   this host before any scenario judged it.
# - `<scenarios-dir>/build`, when present, builds each tree (run inside it).
# - With a task scenario: it must fail on the base and pass on the candidate.
# - Every other scenario in the directory must pass on the candidate, except
#   ratchet scenarios (a line `# rsi: ratchet`), which compare against the
#   base and so fail on any tree that did not improve.
#
# Scenarios run inside the tree with SPAWN_BIN (the tree's `spawn`, when the
# build made one) and SPAWN_BASELINE_TREE set. Their output is printed with a
# `| ` prefix. The verdict is one JSON line, last, on stdout; when the
# candidate's task run printed a `METRIC` line, the verdict carries it as
# "metric" (the controller reads the metric from the verdict only).
#
# ponytail: the gate runs on the host. Move it into a container without the
# `spawn` binary when a candidate is treated as untrusted.

set -eu

repo=$1
baseline=$2
candidate=$3
scenarios=$4
scenario=${5-}

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

metric=""

# verdict <accepted|rejected> [reason]
verdict() {
    json="{\"verdict\":\"$1\""
    [ -n "${2-}" ] && json="$json,\"reason\":\"$2\""
    [ -n "$metric" ] && json="$json,\"metric\":\"$metric\""
    echo "$json}"
}

# The controller resolves these paths against its own checkout. A relative
# path would resolve inside the tree under test.
for path in "$scenarios" ${scenario:+"$scenario"}; do
    case "$path" in
    /*) ;;
    *)
        verdict rejected "scenario path is not absolute"
        exit 0
        ;;
    esac
done
if [ ! -d "$scenarios" ]; then
    verdict rejected "scenarios directory is missing"
    exit 0
fi
if [ -n "$scenario" ] && [ ! -f "$scenario" ]; then
    verdict rejected "scenario is missing"
    exit 0
fi

extract() {
    mkdir -p "$2"
    git --git-dir="$repo" archive "$1" | tar -x -C "$2"
}

# 0 = built (or nothing to build), 2 = did not build.
build() {
    [ -f "$scenarios/build" ] || return 0
    (cd "$1" && sh "$scenarios/build") >"$work/build.log" 2>&1 || {
        sed 's/^/| build: /' "$work/build.log" | tail -n 30
        return 2
    }
}

# Run one scenario in a built tree, echoing its output prefixed with `| `.
run_scenario() {
    status=0
    bin="$1/target/debug/spawn"
    (cd "$1" && SPAWN_BIN="$bin" SPAWN_BASELINE_TREE="$work/baseline" CARGO_TARGET_DIR="$1/target" \
        sh "$2") >"$work/run.log" 2>&1 || status=$?
    sed 's/^/| /' "$work/run.log" | tail -n 60
    return "$status"
}

changed=$(git --git-dir="$repo" diff --name-only "$baseline" "$candidate")

touched=""
if [ -f "$scenarios/protected" ]; then
    touched=$(printf '%s\n' "$changed" | awk -v list="$scenarios/protected" '
        BEGIN { while ((getline line < list) > 0) if (line != "" && line !~ /^#/) prefix[++n] = line }
        { for (i = 1; i <= n; i++) if (index($0, prefix[i]) == 1) { print; break } }')
fi
if [ -n "$touched" ]; then
    echo "| candidate touches protected paths:$(echo "$touched" | tr '\n' ' ')"
    verdict rejected "candidate touches protected paths"
    exit 0
fi

# Build scripts and cargo configuration run code at build time, on this host,
# before any scenario judges the candidate. No task needs them; refuse them.
buildtime=$(printf '%s\n' "$changed" | grep -E '(^|/)build\.rs$|(^|/)\.cargo/' || true)
if [ -n "$buildtime" ]; then
    echo "| candidate adds build-time code: $(echo "$buildtime" | tr '\n' ' ')"
    verdict rejected "candidate adds build-time code (build.rs or .cargo/)"
    exit 0
fi

extract "$baseline" "$work/baseline"
extract "$candidate" "$work/candidate"

if [ -n "$scenario" ]; then
    echo "| == base: $(basename "$scenario") must fail"
    if ! build "$work/baseline"; then
        verdict rejected "base does not build"
        exit 0
    fi
    red=0
    run_scenario "$work/baseline" "$scenario" || red=$?
    if [ "$red" -eq 0 ]; then
        verdict rejected "scenario already passes on the base"
        exit 0
    fi
fi

echo "| == candidate: build"
if ! build "$work/candidate"; then
    verdict rejected "candidate does not build"
    exit 0
fi

if [ -n "$scenario" ]; then
    echo "| == candidate: $(basename "$scenario") must pass"
    green=0
    rm -f "$work/run.log"
    run_scenario "$work/candidate" "$scenario" || green=$?
    # The scenario prints its METRIC line last; candidate code ran before it.
    metric=$(grep -E '^METRIC [A-Za-z0-9_]+( [a-z]+=[0-9.]+)*$' "$work/run.log" 2>/dev/null | tail -n 1 || true)
    if [ "$green" -ne 0 ]; then
        verdict rejected "scenario fails on the candidate"
        exit 0
    fi
fi

for other in "$scenarios"/*.sh; do
    [ -f "$other" ] || continue
    [ "$other" = "$scenario" ] && continue
    grep -q '^# rsi: ratchet' "$other" && continue
    echo "| == regression: $(basename "$other") must still pass"
    if ! run_scenario "$work/candidate" "$other"; then
        verdict rejected "regression: $(basename "$other") fails on the candidate"
        exit 0
    fi
done
verdict accepted
