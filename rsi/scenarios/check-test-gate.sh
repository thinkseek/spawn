#!/bin/sh
# Check: the gate's own tests, scripts/rsi/test-gate.sh, pass on a tree that
# changes the gate. No gate ran them before: every change to the gate was
# tested only by whoever ran them by hand, as test-maintenance was never
# run at all until check-maintenance (batch 13, 2026-11-09).
#
# Only when the tree differs from the base (SPAWN_BASELINE_TREE) in what the
# gate runs or its tests read: scripts/rsi/ (gate.sh, gate-pg.sh and the
# tests themselves), the helpers gate.sh sources (scripts/disk.sh,
# scripts/stage.sh, scripts/pg-url.sh), the two scenario files the tests
# copy (rsi/scenarios/protected, check-schema-upgrade.sh) and the helper
# that check-schema-upgrade.sh calls in test 23
# (scripts/blackbox/scenario-db.sh). Otherwise it passes at once, so a tree
# that leaves the gate alone pays nothing. On the base itself nothing differs: a candidate
# that changes the gate is the one to leave its tests green.
#
# What it costs when it runs: the whole suite as one run of the candidate
# phase, 1,000 to 1,584 s inside a gate on this host (2026-11-09, 38fda305
# against 37be5787, load 5 to 19), so a gate that changes the gate takes
# about 17 to 26 minutes more where nothing else is as long; its scratch
# peaked at 348 MB, on disk. It runs gates of its own, in sandboxes nested
# in this one, with a home, caches and host slots of their own, and the
# host's PostgreSQL programs when it has them.
set -eu
[ -f scripts/rsi/test-gate.sh ] || exit 0
base=${SPAWN_BASELINE_TREE-}
if [ -n "$base" ] && [ -d "$base" ]; then
    same=1
    for path in scripts/rsi scripts/disk.sh scripts/stage.sh scripts/pg-url.sh rsi/scenarios/protected \
        rsi/scenarios/check-schema-upgrade.sh scripts/blackbox/scenario-db.sh; do
        if [ -e "$path" ] || [ -e "$base/$path" ]; then
            diff -rq -- "$base/$path" "$path" >/dev/null 2>&1 || {
                same=""
                echo "changed against the base: $path"
            }
        fi
    done
    if [ -n "$same" ]; then
        echo "the gate and its tests are the base's: not run"
        exit 0
    fi
fi
# Where it runs: a nested gate refuses to run its scripts from under /tmp,
# /run or /var/tmp (the roots its own sandbox empties), and the tree is
# under the gate's TMPDIR (/var/tmp/spawn on this host), so the tests run
# from a copy of the tree's scripts/ and rsi/scenarios/ (which they read) at $HOME/test-gate, a path no gate empties, with their scratch
# beside it. That path is the sandbox's own /tmp/test-gate (on disk, in the
# gate's scratch) mounted there by a bwrap of its own: the sandbox's $HOME
# is a tmpfs, and the suite's trees and builds are not to be held in memory.
mkdir -p /tmp/test-gate "$HOME/test-gate"
exec bwrap --dev-bind / / --bind /tmp/test-gate "$HOME/test-gate" --chdir "$PWD" sh -c '
    set -eu
    root=$HOME/test-gate
    mkdir -p "$root/tree/rsi" "$root/tmp"
    cp -a scripts "$root/tree/"
    cp -a rsi/scenarios "$root/tree/rsi/"
    TMPDIR=$root/tmp exec sh "$root/tree/scripts/rsi/test-gate.sh"
'
