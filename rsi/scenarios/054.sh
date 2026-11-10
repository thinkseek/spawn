#!/bin/sh
# Black box: a full `cargo test` leaves no scratch of its own in TMPDIR,
# apart from a failing test's own directory.
#
# The workspace's tests make their scratch under std::env::temp_dir()
# (`spawn-git-<tag>-<pid>`, `spawn-branches-<tag>-<pid>`,
# `spawn-host-test-<pid>`, `spawn-runtime-*`, `spawn-gate-*`,
# `spawn-db-*`, `spawn-land-push-*`, ...; 66 sites name `spawn-<...>-<pid>`
# under temp_dir alone) and many never remove it. Measured on 351fb1e6
# (2026-11-11, each crate's tests alone, a fresh TMPDIR each, all passing):
#   spawn-host  117 tests, 14 s: 51 entries left, 54 MB
#   spawn-cli   715 tests, 115 s: 7 entries left, 2 MB
#   spawn-core, spawn-postgres, spawn-kdf, spawn-jev: none.
# Three of spawn-cli's are the product's own: a halted gate (an integrator's
# drain) parks its thread for good, and its private directory
# (`spawn-gate-<pid>-...`) is never removed; /var/tmp/spawn holds 76.
# rev-b's suite TMPDIR held 705 entries (714 MB) after 13 crate-suite runs
# in ten minutes; /var/tmp/spawn held 6042 entries (5.3 GB). Each entry is
# also walked by the gate's socket scan.
#
# Required: every test's scratch is removed when the test ends, kept only
# when the test fails (for debugging); a halted gate removes its private
# directory before its thread parks. A `cargo test` whose tests all pass
# leaves no new `spawn-*` entry in its TMPDIR.
#
# To keep its cost per gate small, the scenario runs only the two crates
# that leak, `cargo test -p spawn-host -p spawn-cli` (about 130 s; the build
# is the gate's, CARGO_TARGET_DIR as the gate gives it), in the tree under
# test, with TMPDIR a fresh empty directory of its own, and lists what is
# left:
#   (1) the tests pass (a failing suite cannot be judged for its scratch;
#       the check of the tests reports it);
#   (2) no `spawn-*` entry is left in that TMPDIR.
# Entries of other names (the tools' own) are listed, not judged.
# Once this is solved, the owner folds the same check into the check of the
# tests (every crate) and retires this scenario.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.
set -eu

asserted=""
harness() {
    echo "054: harness: $*" >&2
    exit 2
}
note() { echo "054: $*" >&2; }

[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required (spawn-cli's tests use the test database; they do not skip)"
for tool in cargo find sort sed awk du grep; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done
tree=$(pwd)
[ -f "$tree/Cargo.toml" ] && [ -d "$tree/crates/spawn-host" ] && [ -d "$tree/crates/spawn-cli" ] ||
    harness "run from the tree under test: no crates/spawn-host and crates/spawn-cli under $tree"

WORK=$(mktemp -d) || harness "mktemp failed"
finish() {
    rc=$?
    trap - EXIT
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "054: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

fail() {
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}

# --- the suite ---------------------------------------------------------------------

scratch=$WORK/tmp
mkdir -p "$scratch"
s=$(date +%s)
set +e
TMPDIR=$scratch cargo test --quiet -p spawn-host -p spawn-cli >"$WORK/test.log" 2>&1
rc=$?
set -e
took=$(( $(date +%s) - s ))
left=$(find "$scratch" -mindepth 1 -maxdepth 1 -name 'spawn-*' -printf '%f\n' | sort)
others=$(find "$scratch" -mindepth 1 -maxdepth 1 ! -name 'spawn-*' -printf '%f\n' | sort | head -n 20 | tr '\n' ' ')
[ -z "$others" ] || note "entries of other names left in TMPDIR (not judged): $others"
if [ "$rc" -ne 0 ]; then
    grep -E '^test .* FAILED|^error|panicked' "$WORK/test.log" | head -n 15 >&2 || true
    fail "(1) cargo test -p spawn-host -p spawn-cli failed (exit $rc, ${took} s): a failing suite cannot be judged for its scratch; its spawn-* entries: $(printf '%s' "$left" | wc -l)"
fi
note "(1) cargo test -p spawn-host -p spawn-cli passed in ${took} s"
count=$(printf '%s' "$left" | grep -c . || true)
if [ "$count" -gt 0 ]; then
    size=$(du -sm "$scratch" | cut -f1)
    by=$(printf '%s\n' "$left" | sed -E 's/-[0-9]+$//' | sort | uniq -c | sort -rn | head -n 12 | awk '{printf "%s x%s, ", $2, $1}')
    fail "(2) a passing cargo test -p spawn-host -p spawn-cli left $count spawn-* entries (${size} MB) in its TMPDIR: $by..."
fi
note "(2) no spawn-* entry left in TMPDIR"

echo "054: the tests of spawn-host and spawn-cli, all passing, leave no spawn-* scratch in their TMPDIR"
exit 0
