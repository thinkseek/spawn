#!/bin/bash
# test-sweep-test-schemas.sh: sweep-test-schemas.sh is print-only and keeps
# what it must (P21). Needs SPAWN_TEST_DATABASE_URL (a disposable database);
# it makes three probe schemas of its own and drops exactly those, by name.
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || { echo "SPAWN_TEST_DATABASE_URL is required" >&2; exit 2; }
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" || exit 2
failed=0
expect() { # <what> <output> <line pattern (grep -E)>
    # A here-string, not a pipe: under pipefail, grep -q closing early
    # would fail the printf.
    if grep -qE "$3" <<<"$2"; then echo "ok   $1"; else echo "FAIL $1: no line /$3/"; failed=$((failed + 1)); fi
}
# A pid no process can have (above pid_max), this shell's (alive), and none.
dead=$(($(cat /proc/sys/kernel/pid_max) + 1))
tag=$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')
gone="spawn_test_${dead}_${tag}_sweepprobe"
alive="spawn_test_$$_${tag}_sweepprobe"
nopid="spawn_test_sweepprobe_$(printf '%s' "$tag" | tr 0-9_ a-jx)"
cleanup() {
    for name in "$gone" "$alive" "$nopid"; do
        psql -X -q -c "drop schema if exists $name cascade" >/dev/null 2>&1
    done
}
trap cleanup EXIT
for name in "$gone" "$alive" "$nopid"; do
    psql -X -q -v ON_ERROR_STOP=1 -c "create schema $name; create table $name.t (x int); insert into $name.t values (1)" >/dev/null || exit 2
done
out=$(bash "$here/sweep-test-schemas.sh" --min-idle-hours 0) || { echo "FAIL the sweep exits $?"; exit 1; }
expect "a schema whose pid is gone and idle long enough would be dropped" "$out" "^would-drop $gone pid=$dead "
expect "one whose pid runs here stays" "$out" "^keep +$alive pid=$$ .* pid $$ runs here$"
expect "one with no pid in its name stays" "$out" "^keep +$nopid pid=- .* no pid in the name$"
expect "it says it dropped nothing" "$out" "^sweep-test-schemas \\(print-only, nothing dropped\\)"
out=$(bash "$here/sweep-test-schemas.sh")
expect "by default (24 h) a schema written just now stays" "$out" "^keep +$gone pid=$dead .*written 0h ago"
left=$(psql -X -qtA -c "select count(*) from pg_namespace where nspname in ('$gone', '$alive', '$nopid')")
expect "print-only: all three are still there" "$left" '^3$'
bash "$here/sweep-test-schemas.sh" --apply >/dev/null 2>&1
expect "there is no other mode (--apply is refused: exit 2)" "$?" '^2$'
[ "$failed" -eq 0 ] && echo "test-sweep-test-schemas: all passed" || echo "test-sweep-test-schemas: $failed FAIL"
[ "$failed" -eq 0 ]
