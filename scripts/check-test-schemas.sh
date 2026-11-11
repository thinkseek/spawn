#!/bin/sh
# check-test-schemas.sh COMMAND [ARG...]: run COMMAND (a crate suite) and say
# whether it left new spawn_test_* schemas in the test database
# SPAWN_TEST_DATABASE_URL names (backlog P19).
#
# The schemas are listed by name before and after COMMAND; any name only in
# the second list is one the run left. A test that failed keeps its schema
# on purpose (it says so on stderr: "the failed test's schema is kept"), so
# a run with failures may leave those; the check names them all the same.
# The database must be this run's alone while it runs (the ccd test
# database, not the fleet's): a schema another process's tests hold at the
# second listing would be counted as left. Nothing is dropped here.
#
# Exit: COMMAND's status when no schema was left; 3 when one was (their
# names on stderr); 2 when the check itself cannot run.
set -eu

[ "$#" -gt 0 ] || { echo "usage: check-test-schemas.sh COMMAND [ARG...]" >&2; exit 2; }
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || { echo "check-test-schemas: SPAWN_TEST_DATABASE_URL is required" >&2; exit 2; }
here=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=scripts/pg-url.sh
. "$here/pg-url.sh"

# The URL becomes psql's PG* variables in a subshell, never an argument.
schemas() {
    (
        pg_url_export "$SPAWN_TEST_DATABASE_URL" || exit 2
        names=$(psql -X -qtA -v ON_ERROR_STOP=1 \
            -c "select nspname from pg_namespace where nspname like 'spawn\_test\_%'") || exit 2
        [ -z "$names" ] || printf '%s\n' "$names" | LC_ALL=C sort
    )
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/check-test-schemas.XXXXXX")
trap 'rm -f "$scratch/before" "$scratch/after"; rmdir "$scratch"' EXIT
schemas >"$scratch/before" || { echo "check-test-schemas: cannot list the schemas" >&2; exit 2; }
status=0
"$@" || status=$?
schemas >"$scratch/after" || { echo "check-test-schemas: cannot list the schemas" >&2; exit 2; }
left=$(LC_ALL=C comm -13 "$scratch/before" "$scratch/after")
if [ -n "$left" ]; then
    echo "check-test-schemas: the run left $(printf '%s\n' "$left" | wc -l | tr -d ' ') schema(s):" >&2
    printf '%s\n' "$left" >&2
    exit 3
fi
echo "check-test-schemas: no new spawn_test_* schema (exit $status)" >&2
exit "$status"
