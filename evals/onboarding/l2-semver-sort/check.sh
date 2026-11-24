#!/bin/sh
# Exam l2-semver-sort: sh check.sh <workspace>. Exit 0 pass, 1 fail, 2 harness.
set -u
ws=${1:?usage: check.sh <workspace>}
LC_ALL=C
export LC_ALL
[ -f "$ws/semver_sort.sh" ] || { echo "l2-semver-sort: no semver_sort.sh"; exit 1; }
tmp=$(mktemp -d) || exit 2
trap 'rm -rf "$tmp"' EXIT
run() {
    if command -v timeout >/dev/null 2>&1; then timeout 20 sh "$@"; else sh "$@"; fi
}
# case <name>: the input in $tmp/in, the wanted output in $tmp/want.
case_() {
    run "$ws/semver_sort.sh" <"$tmp/in" >"$tmp/got" 2>"$tmp/err" || {
        echo "l2-semver-sort: $1: exited non-zero"; exit 1; }
    cmp -s "$tmp/want" "$tmp/got" || {
        echo "l2-semver-sort: $1: wrong order"; diff "$tmp/want" "$tmp/got" | head -n 10; exit 1; }
}
# The specification's own precedence chain, shuffled.
printf '%s\n' 1.0.0 1.0.0-rc.1 1.0.0-alpha.beta 1.0.0-beta.11 1.0.0-alpha \
    1.0.0-beta.2 1.0.0-beta 1.0.0-alpha.1 >"$tmp/in"
printf '%s\n' 1.0.0-alpha 1.0.0-alpha.1 1.0.0-alpha.beta 1.0.0-beta 1.0.0-beta.2 \
    1.0.0-beta.11 1.0.0-rc.1 1.0.0 >"$tmp/want"
case_ precedence
# Numeric parts as numbers, not text.
printf '%s\n' 10.0.0 2.0.0 1.10.0 1.9.0 1.0.10 1.0.9 11.0.0 >"$tmp/in"
printf '%s\n' 1.0.9 1.0.10 1.9.0 1.10.0 2.0.0 10.0.0 11.0.0 >"$tmp/want"
case_ numeric
# Numbers longer than any shell or float integer.
printf '%s\n' 123456789012345678901234567890.0.0 99999999999999999999999999999.0.0 \
    123456789012345678901234567889.0.0 >"$tmp/in"
printf '%s\n' 99999999999999999999999999999.0.0 123456789012345678901234567889.0.0 \
    123456789012345678901234567890.0.0 >"$tmp/want"
case_ big
# Numeric below alphanumeric, ASCII order, hyphens in identifiers, duplicates
# kept (an input has no build metadata, so versions of equal precedence are
# the same line: their order cannot be seen, and is not checked).
printf '%s\n' 1.0.0-a 1.0.0-1 1.0.0-B 1.0.0-a-b 1.0.0-10 1.0.0-2 2.0.0 2.0.0 \
    1.0.0-1.a 1.0.0-1.1 >"$tmp/in"
printf '%s\n' 1.0.0-1 1.0.0-1.1 1.0.0-1.a 1.0.0-2 1.0.0-10 1.0.0-B 1.0.0-a \
    1.0.0-a-b 2.0.0 2.0.0 >"$tmp/want"
case_ identifiers
: >"$tmp/in"
: >"$tmp/want"
case_ empty
echo "l2-semver-sort: pass"
