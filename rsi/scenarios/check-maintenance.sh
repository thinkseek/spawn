#!/bin/sh
# Check: the tree's scripts/test-maintenance.sh passes on its own
# scripts/maintenance.sh, the hourly cleanup (spawn-maintenance.timer) that
# deletes what Spawn wrote: the gate's scratch, old deploy dumps, base
# results and the private test servers' templates. Nothing ran that test
# before: maintenance.sh's step 4 removed every template under ~/.cache, in
# use or not, on every run, unseen until a gate's two server starts got two
# passwords (batch 13, 2026-11-09). It runs on a throwaway TMPDIR and cache
# of its own, never the owner's.
set -eu
[ -f scripts/test-maintenance.sh ] || exit 0
sh scripts/test-maintenance.sh
