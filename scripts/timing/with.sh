#!/usr/bin/env bash
# Run one command (an RSI scenario, a check) under adversarial timing:
# shims first in PATH and SPAWN_BIN the spawn wrapper (scripts/timing/timing.sh).
#
# usage: SPAWN_BIN=... [SPAWN_TIMING_SEED=<n>] [SPAWN_TIMING_KINDS=...] \
#            scripts/timing/with.sh [-d <timing-dir>] <command> [<argument> ...]
#
# The timing directory defaults to a new one under TMPDIR, removed afterwards
# (its perturbation log is summarised on standard error first). Exits with
# the command's status. Without SPAWN_TIMING=adversarial in the environment
# the mode is still on: calling this script is the opt-in.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
dir=""
keep=""
if [ "${1:-}" = -d ]; then
    dir=$2
    keep=1
    shift 2
fi
[ $# -gt 0 ] || { sed -n '2,11p' "$0" >&2; exit 2; }
: "${SPAWN_BIN:?SPAWN_BIN is required}"
[ -n "$dir" ] || dir=$(mktemp -d "${TMPDIR:-/tmp}/spawn-timing.XXXXXX") || exit 2

# shellcheck source=scripts/timing/timing.sh
. "$root/scripts/timing/timing.sh"
timing_enable "$dir" "$SPAWN_BIN" || exit 2
SPAWN_BIN=$TIMING_SPAWN
SPAWN_TIMING=adversarial
export SPAWN_BIN SPAWN_TIMING
"$@"
rc=$?
timing_report "$dir"
pkill -f "python3 -I $dir/mirror.py" 2>/dev/null || true
[ -n "$keep" ] || rm -rf "$dir"
exit "$rc"
