#!/bin/sh
# Exam l3-toposort: sh check.sh <workspace>. Exit 0 pass, 1 fail, 2 harness.
# Each cases/NN.in is run; its output must equal NN.out and its exit
# status NN.code.
set -u
ws=${1:?usage: check.sh <workspace>}
here=$(cd "$(dirname "$0")" && pwd) || exit 2
LC_ALL=C
export LC_ALL
[ -f "$ws/toposort.sh" ] || { echo "l3-toposort: no toposort.sh"; exit 1; }
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
run() {
    if command -v timeout >/dev/null 2>&1; then timeout 20 sh "$@"; else sh "$@"; fi
}
found=0
for input in "$here"/cases/*.in; do
    [ -f "$input" ] || exit 2
    found=$((found + 1))
    name=${input%.in}
    code=0
    run "$ws/toposort.sh" <"$input" >"$tmp/got" 2>"$tmp/err" || code=$?
    want=$(cat "$name.code") || exit 2
    if [ "$code" != "$want" ]; then
        echo "l3-toposort: $(basename "$name"): exit $code, not $want"; exit 1
    fi
    cmp -s "$name.out" "$tmp/got" || {
        echo "l3-toposort: $(basename "$name"): wrong output"
        diff "$name.out" "$tmp/got" | head -n 10; exit 1; }
done
[ "$found" -gt 0 ] || exit 2
echo "l3-toposort: pass"
