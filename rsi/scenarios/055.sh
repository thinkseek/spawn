#!/bin/sh
# Black box: a crate-suite run leaves no schema of its own in the test
# database.
#
# The database tests run against SPAWN_TEST_DATABASE_URL, each case in a
# schema of its own (`spawn_test_<pid>_<nanos>_<case>`), which
# crates/spawn-postgres/tests/common's `Schema` guard drops when the case
# ends. Not every case takes the guard: roles.rs's
# many_migrations_at_once_on_one_cluster_all_succeed makes eight
# (`..._burst_0` to `_7`) through `schema_url` alone, and nothing drops
# them. Measured on 351fb1e6 (2026-11-11, each crate's tests alone against a
# fresh database of their own, all passing): spawn-postgres leaves 8
# schemas; spawn-cli, spawn-core, spawn-host, spawn-kdf and spawn-jev leave
# none. On the fleet's test database (fleet-pg, spawn_fleet_test) 2533 such
# schemas had piled up, 2.7 GB with their catalog rows, by 11-11 (the lead
# dropped them; 128 MB after).
#
# Required: a crate-suite run whose tests pass leaves no schema it made in
# the test database, of any name (a schema renamed is a schema all the
# same); every case drops what it made, the cases that make several
# included.
#
# To keep its cost per gate small, the scenario runs only the crate that
# leaks, `cargo test -p spawn-postgres` (about 20 s; the build is the
# gate's, CARGO_TARGET_DIR as the gate gives it), in the tree under test,
# with SPAWN_TEST_DATABASE_URL a fresh empty database of its own on the
# same server, and then lists the schemas in it:
#   (1) the tests pass (a failing suite cannot be judged for what it
#       leaves; the check of the tests reports it);
#   (2) no schema is left but the server's own (public, information_schema,
#       pg_*), whatever its name.
# Once this is solved, the owner folds the same check into the check of the
# tests (every crate) and retires this scenario.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
set -eu

asserted=""
harness() {
    echo "055: harness: $*" >&2
    exit 2
}
note() { echo "055: $*" >&2; }

[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required (the database tests do not skip)"
for tool in cargo psql od tr grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
tree=$(pwd)
[ -f "$tree/Cargo.toml" ] && [ -d "$tree/crates/spawn-postgres" ] ||
    harness "run from the tree under test: no crates/spawn-postgres under $tree"
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/pg-url.sh" ] || harness "no $controller/scripts/pg-url.sh"

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
db="bb_055_schemas_$run"
database_made=""
finish() {
    rc=$?
    trap - EXIT
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$db" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "055: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

fail() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "bb_055_schemas_" >/dev/null 2>&1 || true
psql -d postgres -qtAc "create database $db" >/dev/null 2>"$WORK/create.err" ||
    harness "cannot create the scenario's database: $(head -c 300 "$WORK/create.err")"
database_made=1

# --- the suite ---------------------------------------------------------------------

# The schemas of the scenario's database that are not the server's own.
schemas() {
    psql -d "$db" -qtA -v ON_ERROR_STOP=1 -c "select nspname from pg_namespace
        where nspname not in ('public', 'information_schema') and nspname not like 'pg\\_%'
        order by nspname" 2>"$WORK/q.err" || harness "listing the schemas failed: $(head -c 300 "$WORK/q.err")"
}
before=$(schemas | grep -c . || true)
[ "$before" -eq 0 ] || harness "the fresh database already holds $before schemas"

s=$(date +%s)
set +e
SPAWN_TEST_DATABASE_URL="$server_url/$db" cargo test --quiet -p spawn-postgres >"$WORK/test.log" 2>&1
rc=$?
set -e
took=$(( $(date +%s) - s ))
left=$(schemas)
count=$(printf '%s' "$left" | grep -c . || true)
if [ "$rc" -ne 0 ]; then
    grep -E '^test .* FAILED|^error|panicked' "$WORK/test.log" | head -n 15 >&2 || true
    fail "(1) cargo test -p spawn-postgres failed (exit $rc, ${took} s): a failing suite cannot be judged for what it leaves; schemas in its database: $count"
fi
note "(1) cargo test -p spawn-postgres passed in ${took} s"
if [ "$count" -gt 0 ]; then
    size=$(psql -d "$db" -qtAc "select pg_size_pretty(pg_database_size(current_database()))" 2>/dev/null || echo "?")
    fail "(2) a passing cargo test -p spawn-postgres left $count schemas in its test database (now $size), which held none before: $(printf '%s\n' "$left" | head -n 12 | tr '\n' ' ')..."
fi
note "(2) no schema left in the test database"

echo "055: the tests of spawn-postgres, all passing, leave no schema in their test database"
exit 0
