#!/bin/sh
# Black box (objective 139) [storage]: a failed test's schema goes with the
# test, unless SPAWN_TEST_KEEP_FAILED=1 keeps it for debugging.
#
# The problem: spawn-postgres's TestSchema kept every failed test's schema
# "for debugging". Mutation runs make tests fail on purpose, and each caught
# mutant left one schema on the shared test server, about one every 30 s while
# a reviewer ran mutants: 100 to 270 a day on the ccd server, and 1001 leaked
# in /api/core-lines' storage.leaked_test_schemas on 11-14 (range: max 200).
#
# The world: a database of this run's own, and a scratch crate outside the
# tree whose one test uses the tree's own spawn-postgres (by path, its
# `testing` feature): it makes a TestSchema, has its store make the schema,
# prints the name, and fails on purpose. The crate is built once (offline,
# the tree's Cargo.lock), then its test runs three times against this run's
# database:
#   (1) SPAWN_TEST_KEEP_FAILED unset: the test fails (exit 101, and the
#       binary's own "test result: FAILED. 0 passed; 1 failed", so it did not
#       abort as it unwound; after printing the schema's name, so the schema
#       existed), afterwards no
#       schema of that case is in the database, and its output says "the
#       failed test's schema is dropped: <name> (SPAWN_TEST_KEEP_FAILED=1
#       keeps it)";
#   (2) SPAWN_TEST_KEEP_FAILED=1: the test fails, and afterwards exactly one
#       is, the one it printed;
#   (3) SPAWN_TEST_KEEP_FAILED=yes (not 1): the test fails, and that run's
#       schema is gone too (still exactly (2)'s one).
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "139: harness: $*" >&2
    exit 2
}
check_failed() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"
for tool in cargo psql od; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/pg-url.sh" ] && [ -f "$controller/scripts/blackbox/scenario-db.sh" ] ||
    harness "no scripts/pg-url.sh or scripts/blackbox/scenario-db.sh in $controller"
# The tree under test is the working directory (as in 107.sh): the gate runs
# a scenario from the candidate's tree, and this file's directory is the
# controller's copy.
tree=$(pwd)
[ -f "$tree/Cargo.toml" ] && [ -f "$tree/Cargo.lock" ] && [ -d "$tree/crates/spawn-postgres" ] ||
    harness "run from the tree under test: no Cargo.toml, Cargo.lock and crates/spawn-postgres under $tree"

WORK=$(mktemp -d) || harness "mktemp failed"
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_139_schemas_$run"
database_made=""
cleanup() {
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "139: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")
SPAWN_PSQL=psql
export SPAWN_PSQL
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

# --- the scratch crate: one test that fails on purpose ----------------------------------
crate=$WORK/crate
mkdir -p "$crate/src" "$crate/tests"
cat >"$crate/Cargo.toml" <<EOF
[package]
name = "bb139"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dev-dependencies]
spawn-postgres = { path = "$tree/crates/spawn-postgres", features = ["testing"] }
EOF
: >"$crate/src/lib.rs"
cat >"$crate/tests/fails.rs" <<'EOF'
#[test]
fn fails_on_purpose() {
    let schema = spawn_postgres::testing::TestSchema::new("bb139_failed");
    drop(schema.store());
    println!("BB139 made {}", schema.name());
    panic!("BB139 fails on purpose");
}
EOF
cp "$tree/Cargo.lock" "$crate/Cargo.lock"
CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$WORK/target}
export CARGO_TARGET_DIR
if ! (cd "$crate" && cargo test --offline --quiet --no-run) >"$WORK/build.out" 2>&1; then
    grep -E '^error' -A5 "$WORK/build.out" | head -n 30 >&2
    harness "the scratch crate does not build against the tree's spawn-postgres"
fi

# count: schemas of the case in this run's database
count() {
    psql -d "$SPAWN_DB_NAME" -tA -c "select count(*) from pg_namespace where nspname like 'spawn\\_test\\_%\\_bb139\\_failed'" ||
        harness "cannot count the schemas"
}
# fail <keep: unset or a value> <case>: one run of the failing test; its output in $WORK/run.out
fail() {
    if [ "$1" = unset ]; then
        (cd "$crate" && unset SPAWN_TEST_KEEP_FAILED && SPAWN_TEST_DATABASE_URL="$server_url/$SPAWN_DB_NAME" \
            cargo test --offline --quiet -- --nocapture) >"$WORK/run.out" 2>&1 && code=0 || code=$?
    else
        (cd "$crate" && SPAWN_TEST_KEEP_FAILED="$1" SPAWN_TEST_DATABASE_URL="$server_url/$SPAWN_DB_NAME" \
            cargo test --offline --quiet -- --nocapture) >"$WORK/run.out" 2>&1 && code=0 || code=$?
    fi
    # A test that failed and a binary that finished: cargo exits 101 for
    # both a failed test and a test binary that aborted (a Drop that panics
    # as the test unwinds aborts it, SIGABRT), so the binary's own summary
    # line is what tells them apart. An abort is a wrong behaviour.
    [ "$code" -eq 101 ] || check_failed "(${2:-?}) the failing test exited $code, not 101 (a test failure): $(tail -n 5 "$WORK/run.out" | tr '\n' ' ')"
    grep -q '^test result: FAILED. 0 passed; 1 failed' "$WORK/run.out" ||
        check_failed "(${2:-?}) the test binary did not finish with its one test failed (an abort as the test unwound?): $(grep -m1 -E 'signal|panicked' "$WORK/run.out" | cut -c1-160)"
    made=$(sed -n 's/^BB139 made \(spawn_test_[a-z0-9_]*\)$/\1/p' "$WORK/run.out" | head -n 1)
    [ -n "$made" ] || harness "the test did not make its schema: $(tail -n 5 "$WORK/run.out" | tr '\n' ' ')"
    grep -q 'BB139 fails on purpose' "$WORK/run.out" || harness "the test did not fail on purpose"
}

[ "$(count)" = 0 ] || harness "the scenario database already holds the case's schemas"

# --- (1) unset: no schema left ----------------------------------------------------------
fail unset 1
left=$(count)
[ "$left" = 0 ] || check_failed "(1) without SPAWN_TEST_KEEP_FAILED, the failed test left $left schema(s) ($made), not 0"
grep -q "the failed test's schema is dropped: $made (SPAWN_TEST_KEEP_FAILED=1 keeps it)" "$WORK/run.out" ||
    check_failed "(1) the failed test's output does not say its schema $made is dropped and how to keep it"
echo "139: (1) checked: a failed test's schema goes with it"

# --- (2) =1: exactly the one it made ----------------------------------------------------
fail 1 2
kept=$made
left=$(count)
[ "$left" = 1 ] || check_failed "(2) with SPAWN_TEST_KEEP_FAILED=1, the failed test left $left schema(s), not 1"
[ "$(psql -d "$SPAWN_DB_NAME" -tA -c "select count(*) from pg_namespace where nspname = '$kept'")" = 1 ] ||
    check_failed "(2) the schema left is not the one the test made ($kept)"
grep -q "kept" "$WORK/run.out" || check_failed "(2) the test's output does not say its schema is kept"
echo "139: (2) checked: SPAWN_TEST_KEEP_FAILED=1 keeps exactly the failed test's schema"

# --- (3) another value: not kept ---------------------------------------------------------
fail yes 3
left=$(count)
[ "$left" = 1 ] || check_failed "(3) with SPAWN_TEST_KEEP_FAILED=yes, the case's schemas are $left, not (2)'s 1 (this run's $made was kept)"
echo "139: (3) checked: only 1 keeps"

echo "139: a failed test's schema goes with it, unless SPAWN_TEST_KEEP_FAILED=1"
exit 0
