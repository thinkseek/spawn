#!/bin/sh
# Exam l1-slugify: sh check.sh <workspace>. Exit 0 pass, 1 fail, 2 harness.
set -u
ws=${1:?usage: check.sh <workspace>}
LC_ALL=C
export LC_ALL
[ -f "$ws/slugify.sh" ] || { echo "l1-slugify: no slugify.sh"; exit 1; }
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
printf '%s\n' '  Hello, World!' '  --Already--slugged--  ' 'MiXeD 123 numbers' '' '!!!' \
    'a_b.c' 'Café au lait' 'trailing dash-' '0-0' >"$tmp/in"
printf '%s\n' 'hello-world' 'already-slugged' 'mixed-123-numbers' '' '' \
    'a-b-c' 'caf-au-lait' 'trailing-dash' '0-0' >"$tmp/want"
run() {
    if command -v timeout >/dev/null 2>&1; then timeout 20 sh "$@"; else sh "$@"; fi
}
run "$ws/slugify.sh" <"$tmp/in" >"$tmp/got" 2>"$tmp/err" || {
    echo "l1-slugify: slugify.sh exited non-zero"; exit 1; }
cmp -s "$tmp/want" "$tmp/got" || {
    echo "l1-slugify: wrong slugs"; diff "$tmp/want" "$tmp/got" | head -n 10; exit 1; }
echo "l1-slugify: pass"
