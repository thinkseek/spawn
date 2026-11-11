#!/bin/sh
# Check: cargo test on the whole workspace, database tests included.
#
# On a failure it prints one line per failing test, `FAILED <test>: <first
# message line>`, before the usual excerpt, so that every failure is on
# record even when there are many (a database crash fails them all at once,
# and the integrator tells a host fault from a regression by reading them).
#
# A test that runs another suite names that suite's failures further down
# its output: the relay self-test runs Python unittest (`FAIL: test_x`,
# `ERROR: test_y`, `AssertionError: ...`). Up to 5 such lines per test are
# printed as `  in <test>: <line>`, all of them before the FAILED lines. The
# gate keeps only the last 60 lines of a scenario's output, so they are the
# first to go and never take a FAILED line's place. The integrator reads
# only the FAILED lines (integrate/host.rs check_tests_failures).
#
# A run whose tests all pass leaves nothing behind (objectives 54 and 55,
# whose scenarios this folds). The tests are built first, outside what is
# judged (below), so the toolchain's own writes are not counted:
# - nothing in its TMPDIR, whatever the name: the run has a fresh TMPDIR of
#   its own, and every entry left in it is named;
# - no schema in its test database but the server's own (public,
#   information_schema, pg_*), whatever the name, and no relation, function
#   or type in public that was not there before the run. The run has a
#   fresh database of its own on the server of SPAWN_TEST_DATABASE_URL, so
#   what the scenarios beside it leave on that server is not blamed on it.
# Each kind is one line, `FAILED leftover-scratch: ...`,
# `FAILED leftover-schema: ...` or `FAILED leftover-public: ...`. None is a
# connection error, so the integrator reads it as the candidate's failure,
# not the host's. The TMPDIR and the database (with the logins init-db
# makes for it, scripts/blackbox/scenario-db.sh) are removed at exit,
# whatever the outcome. Not judged: a failing run's leftovers (a failing
# test may keep its scratch to say where it is), and the logins and
# databases a test makes, which belong to the whole server that other runs
# share.
set -eu
: "${SPAWN_TEST_DATABASE_URL:?the database tests need SPAWN_TEST_DATABASE_URL; they do not skip}"
harness() {
    echo "check-tests: harness: $*" >&2
    exit 2
}
for tool in cargo psql od find du; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
controller=$(cd "$(dirname "$0")/../.." && pwd)
# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
# The run's own TMPDIR is $scratch, one level under the given one (tests
# make sockets under it, and a socket's path is short); the log and the
# lists taken before the run are in $work.
work=$(mktemp -d)
log=$work/log
scratch=$(mktemp -d)
db=""
cleanup() {
    # A test may leave a directory it made unwritable: make it removable first.
    chmod -R u+rwX "$scratch" 2>/dev/null || true
    rm -rf "$work" "$scratch"
    if [ -n "$db" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT
trap 'exit 130' INT TERM
# Not $$ alone: the gate's PID namespace makes it the same small number.
name="check_tests_$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "check_tests_" >/dev/null 2>&1 || true
psql -d postgres -qtAc "create database $name" >/dev/null 2>"$log" ||
    harness "cannot create the run's database: $(head -c 300 "$log")"
db=$name
# The schemas that are not the server's own; what public holds, as
# "<kind> <name>".
schemas() {
    psql -d "$db" -qtA -v ON_ERROR_STOP=1 -c "select nspname from pg_namespace
        where nspname not in ('public', 'information_schema') and nspname not like 'pg\\_%'
        order by nspname" 2>"$log" || harness "listing the schemas failed: $(head -c 300 "$log")"
}
in_public() {
    psql -d "$db" -qtA -v ON_ERROR_STOP=1 -c "select 'relation ' || relname from pg_class where relnamespace = 'public'::regnamespace
        union all select 'function ' || proname from pg_proc where pronamespace = 'public'::regnamespace
        union all select 'type ' || typname from pg_type where typnamespace = 'public'::regnamespace
        order by 1" 2>"$log" || harness "listing public failed: $(head -c 300 "$log")"
}
schemas >"$work/schemas"
in_public >"$work/public"
# The build first, with the given TMPDIR, not the judged one: the compiler
# cache the host's cargo may run as its rustc wrapper (kache) writes a
# warning marker into TMPDIR when it compiles
# (kache-cow-warn-<hash>.hardlink-exdev), so a run that compiled would leave
# it and a run that did not would leave nothing (054.sh's base disagreed with
# itself on this, owner request 131083). Only the tests' own run is judged.
if ! cargo test --quiet --workspace --no-run >"$log" 2>&1; then
    grep -E '^error' -A5 "$log" | head -n 40
    exit 1
fi
if ! SPAWN_TEST_DATABASE_URL="$server_url/$db${pg_url_query:+?$pg_url_query}" TMPDIR="$scratch" \
    cargo test --quiet --workspace >"$log" 2>&1; then
    awk '
        function flush() {
            if (name != "") failed = failed sprintf("FAILED %s: %s%s\n", name, at, msg)
            name = ""; at = ""; msg = ""
        }
        /^---- .* stdout ----$/ { flush(); name = $2; next }
        /^(failures:|test result:)/ { flush(); next }
        name != "" && at == "" && /panicked at/ { sub(/^thread .* panicked at /, ""); at = $0 " "; next }
        name != "" && msg == "" && NF { msg = substr($0, 1, 200); next }
        name != "" && /^(FAIL|ERROR): |Error: |Error\(/ && kept[name]++ < 5 { printf "  in %s: %s\n", name, substr($0, 1, 200) }
        END { flush(); printf "%s", failed }
    ' "$log"
    grep -E 'FAILED|panicked|^error' -A5 "$log" | head -n 40
    exit 1
fi
# added <before file> <after list>: the lines of the list the file lacks,
# the first 12 on one line.
added() { printf '%s\n' "$2" | grep -vxF -f "$1" | grep . | head -n 12 | tr '\n' ' '; }
leftover=""
left=$(find "$scratch" -mindepth 1 -maxdepth 1 -printf '%f\n' | LC_ALL=C sort)
if [ -n "$left" ]; then
    count=$(printf '%s\n' "$left" | grep -c .)
    size=$(du -sh "$scratch" | cut -f1)
    echo "FAILED leftover-scratch: a passing cargo test --workspace left $count entries ($size) in its TMPDIR, which every test removes whatever the name: $(printf '%s\n' "$left" | head -n 12 | tr '\n' ' ')"
    leftover=1
fi
now=$(schemas)
if [ -n "$(added "$work/schemas" "$now")" ]; then
    echo "FAILED leftover-schema: a passing cargo test --workspace left schemas in its test database, which every case drops whatever the name: $(added "$work/schemas" "$now")"
    leftover=1
fi
now=$(in_public)
if [ -n "$(added "$work/public" "$now")" ]; then
    echo "FAILED leftover-public: a passing cargo test --workspace left objects in public of its test database: $(added "$work/public" "$now")"
    leftover=1
fi
[ -z "$leftover" ] || exit 1
