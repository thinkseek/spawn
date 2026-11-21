#!/bin/sh
# Replay the task-490 dry run against a mission mirror.  Read-only: this
# script runs no check, builds nothing and writes no cache.  It exists so the
# numbers in docs/gate-impact-policy.md can be produced again from the same
# commits and the same measured timings.
#
#   sh rsi/gate-impact/replay.sh [--json] > evidence.txt
#
# The mirror is $SPAWN_GATE_IMPACT_REPO, default the local Spawn mirror.  The
# timings in timings.json were taken from these gate logs (one run each):
#   243: ws/gate-logs/01a1fe88.../spawn-gate-2401779-18ea16198a19e3c8-0
#   485: ws/gate-logs/01a1fec0.../spawn-gate-2503213-18ea19776e34b1e2-1

set -eu
here=$(cd "$(dirname "$0")" && pwd)
repo=${SPAWN_GATE_IMPACT_REPO:-$HOME/.local/share/spawn-rsi/missions/spawn.git}
fmt=${1:-}
if [ "$fmt" = "--json" ]; then fmt="--format json"; else fmt=""; fi

run() {
    label=$1
    shift
    printf '\n### %s\n' "$label"
    # shellcheck disable=SC2086
    python3 "$here/gate_impact.py" --repo "$repo" $fmt "$@"
}

printf '# gate-impact dry run\n'
printf '# date: %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
printf '# host: %s\n' "$(uname -n)"
printf '# mirror: %s\n' "$repo"
printf '# policy: %s\n' "$here/policy.json"

run "243 with no mode flag: the policy's own defaults (cache-only, unsolved run)" \
    --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
    --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --profile 243 \
    --base-record "$here/records/base-5e416443.json"

run "243 proposed: historical objectives nightly, unsolved carried" \
    --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
    --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --profile 243 \
    --base-record "$here/records/base-5e416443.json" \
    --historical-mode nightly --unsolved-mode carry

run "243 cache-only plus carry: no check retired, content-addressed results" \
    --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
    --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --profile 243 \
    --base-record "$here/records/base-5e416443.json" \
    --historical-mode cache-only --unsolved-mode carry

run "243 strict cache: an uncarried unsolved objective is re-judged" \
    --base 5e416443d48c5639456ecb377ae4f214ec05c11c \
    --candidate 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --profile 243 \
    --base-record "$here/records/base-5e416443.json" \
    --historical-mode cache-only --unsolved-mode run

run "485 rust change plus its own task scenario 241 (proposed policy)" \
    --base 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --candidate 33777df8fe02d26c3e2538ba136983eefffa1b36 \
    --profile 485 \
    --base-record "$here/records/base-865cc4df.json" \
    --historical-mode nightly --unsolved-mode carry \
    --task-scenario 241.sh --task-base-cached

run "485 under today's checks plus cache (no retirement)" \
    --base 865cc4df9f88b1c594a78a2aff3a853604930eb0 \
    --candidate 33777df8fe02d26c3e2538ba136983eefffa1b36 \
    --profile 485 \
    --base-record "$here/records/base-865cc4df.json" \
    --historical-mode cache-only --unsolved-mode carry \
    --task-scenario 241.sh --task-base-cached

run "shared/runtime change: gate helper and a journey file (2e8781ca)" \
    --base 337c94b52440198d2bb309e6e20f1043e3b2c6d2 \
    --candidate 2e8781ca \
    --profile 243
