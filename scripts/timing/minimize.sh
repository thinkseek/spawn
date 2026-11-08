#!/usr/bin/env bash
# The smallest set of perturbation kinds that still fails a target under a
# seed: the target under all kinds, then under each kind alone, each
# `-r` times (default 2), so a failure can be told from a flake.
#
# usage: scripts/timing/minimize.sh [-r <repeats>] [-o <dir>] <target> <seed> [<kind> ...]
#
# Kinds default to every kind (scripts/timing/timing.sh). Prints one line per
# kind set: how many of the repeats failed, and the first failure's message.
# Needs what scripts/timing/run.sh needs.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
repeats=2
out=""
while getopts "r:o:" option; do
    case "$option" in
    r) repeats=$OPTARG ;;
    o) out=$OPTARG ;;
    *) sed -n '2,10p' "$0" >&2; exit 2 ;;
    esac
done
shift $((OPTIND - 1))
[ $# -ge 2 ] || { sed -n '2,10p' "$0" >&2; exit 2; }
target=$1
seed=$2
shift 2
kinds=("$@")
[ ${#kinds[@]} -gt 0 ] || kinds=(checkpoint push tmux flaky screen transcript integrate)
out=${out:-${TMPDIR:-/tmp}/spawn-timing-min-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out"

for set in all "${kinds[@]}"; do
    failures=0
    first=""
    for repeat in $(seq 1 "$repeats"); do
        dir=$out/$set-$repeat
        bash "$root/scripts/timing/run.sh" -s "$seed" -k "$set" -o "$dir" "$target" >/dev/null 2>&1
        read -r rc _ <"$dir/$seed/${target//:/-}.result" 2>/dev/null || rc=x
        if [ "$rc" != 0 ]; then
            failures=$((failures + 1))
            if [ -z "$first" ]; then
                first=$(grep -m1 -E 'FAIL|blackbox: |harness: ' "$dir/$seed/${target//:/-}.log" | cut -c1-200)
            fi
        fi
    done
    printf 'minimize: %-22s seed %-4s kinds %-11s failed %s/%s %s\n' \
        "$target" "$seed" "$set" "$failures" "$repeats" "$first"
done
echo "minimize: runs in $out"
