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
# Prints one JSON line.
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
try() {
    tree=$1
    if ! (cd "$tree" && cargo build -q --bin spawn) >"$work/build.log" 2>&1; then
        return 2
    fi
    (cd "$tree" && SPAWN_BIN="$tree/target/debug/spawn" sh "$scenario") \
        >"$work/run.log" 2>&1
}

extract "$baseline" "$work/baseline"
extract "$candidate" "$work/candidate"

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

green=0
try "$work/candidate" || green=$?
if [ "$green" -eq 2 ]; then
    echo '{"verdict":"rejected","reason":"candidate does not build"}'
    exit 0
fi
if [ "$green" -eq 0 ]; then
    echo '{"verdict":"accepted"}'
else
    echo '{"verdict":"rejected","reason":"scenario fails on the candidate"}'
fi
