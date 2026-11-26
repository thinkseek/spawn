#!/bin/sh
# Black box: the drills tests that run a fake systemctl pass every time,
# however they are scheduled beside each other: no run fails on a busy fake
# program, and no test runs another test's fake.
#
# The record (Flake Doctor's real page on FD 016, row `flaky check-tests.sh
# 086ea501 4bedc797 ...
# serve::core_lines::drills::tests::a_unit_call_that_runs_over_is_killed_and_no_pass`):
# in task 328's own gate (judging 01a1d225e6287ca96c5efb6ce7e1, unit
# spawn-gate-1121880-18e770dae00254de-0, stdout line 116) the test failed
# with `.../systemctl: Text file busy (os error 26)` at drills.rs:957, and its
# rerun passed. The gate log proves the errno, not which process held the
# fake open for writing: the historical failure's holder is not identified.
#
# Two defects, each by what was observed (rev-b, 2026-11-13, main 7df39d62,
# the 4 tests below, 16 threads, load about 6, 150 runs each):
# - A, a busy fake. The tests' script() writes the fake in the test process
#   and the test then execs it. A descriptor open for writing on the fake,
#   held by another process while the test execs it, fails the exec with
#   ETXTBSY. The candidate holder is another test thread's child between its
#   fork and its exec. Busy in 10 of 150 runs on main, 18 of 150 with a
#   scratch directory unique per test, 0 of 150 with the fake written by a
#   child process.
# - B, a shared scratch directory. Two tests got one directory
#   (crate::storage::tests::scratch: the pid, the clock's nanoseconds and
#   the case, made with create_dir_all, and every drills test passes the
#   case "drills"), so one ran the other's fake: a_unit_call_that_runs_over
#   answered Ok("Result=success\nExecMainStatus=0\nExecMainExitTimestamp=@5\n"),
#   a_unit_call_is_argv's output. 1 to 4 runs in 150; 0 of 150 with a
#   per-test name.
# Both together fixed (a throwaway): 0 failures in 300 runs.
#
# The run: the spawn-cli tests are built first (not judged). Then the 4
# tests that run a fake systemctl (a_unit_call_is_argv_with_the_unit_after_the_double_dash,
# a_unit_call_that_runs_over_is_killed_and_no_pass,
# a_unit_call_whose_child_holds_the_output_ends_at_its_limit,
# a_refused_name_is_never_run) run together, --test-threads=16, RUNS times
# (default 96), each run with a TMPDIR of its own.
#   (1) every run passes: a failure of any kind fails the check, and its
#       failed tests and messages are printed. (Not a grep for the busy
#       text: a test can swallow the error and fail later on what it
#       caused.)
# This is a statistical check. On main it fails at least 1 run in 15 (7 to
# 12 in 100 measured), so 96 runs pass on the base about 1 time in 1,000 at
# the lowest rate measured.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

harness() {
    echo "107: harness: $*" >&2
    exit 2
}

command -v cargo >/dev/null 2>&1 || harness "cargo is not installed"
# The tree under test is the working directory: the gate runs a scenario
# from the candidate's tree, and the scenario file's own directory is the
# deployed controller's copy (no Cargo.toml above it in the gate, and
# main's tests where there is one), as in 051.sh.
tree=$(pwd)
[ -f "$tree/Cargo.toml" ] && [ -d "$tree/crates/spawn-cli" ] ||
    harness "run from the tree under test: no Cargo.toml and crates/spawn-cli under $tree"
runs=${SPAWN_107_RUNS:-96}
case $runs in '' | 0 | *[!0-9]*) harness "SPAWN_107_RUNS is not a whole number from 1: $runs" ;; esac
p=serve::core_lines::drills::tests::
tests="${p}a_unit_call_is_argv_with_the_unit_after_the_double_dash
${p}a_unit_call_that_runs_over_is_killed_and_no_pass
${p}a_unit_call_whose_child_holds_the_output_ends_at_its_limit
${p}a_refused_name_is_never_run"

WORK=$(mktemp -d)
cleanup() {
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
trap 'status=$?; set +e; cleanup; case $status in 0 | 1 | 2) exit "$status" ;; *) echo "107: harness: unexpected exit $status" >&2; exit 2 ;; esac' EXIT
trap 'exit 130' INT TERM

# The build, outside what is judged.
if ! (cd "$tree" && cargo test --quiet -p spawn-cli --bin spawn --no-run --message-format=json) \
    >"$WORK/build.json" 2>"$WORK/build.err"; then
    grep -E '^error' -A5 "$WORK/build.err" | head -n 40
    echo "107: (1) the spawn-cli tests do not build" >&2
    exit 1
fi
bin=$(grep -o '"executable":"[^"]*"' "$WORK/build.json" | sed 's/^"executable":"//; s/"$//' | grep '/spawn-[0-9a-f]*$' | tail -n 1)
[ -n "$bin" ] && [ -x "$bin" ] || harness "no spawn-cli test executable in cargo's build output"

# Each of the 4 is there, by its exact name.
"$bin" --list --exact $tests >"$WORK/list" 2>&1 || harness "listing the tests failed: $(head -c 300 "$WORK/list")"
for t in $tests; do
    grep -qx "$t: test" "$WORK/list" || {
        echo "107: (1) the test $t is not in spawn-cli's tests (a renamed test is a dropped test)" >&2
        exit 1
    }
done

i=0
while [ "$i" -lt "$runs" ]; do
    i=$((i + 1))
    tmp=$WORK/run-$i
    mkdir "$tmp"
    if TMPDIR=$tmp "$bin" --exact --test-threads=16 $tests >"$WORK/out" 2>&1; then
        rm -rf "$tmp"
        continue
    fi
    echo "107: (1) run $i of $runs failed:" >&2
    grep -E -e '^test .* FAILED$' -e 'panicked at' -e 'Text file busy' -e '^(Ok|Err)\(' -e 'assertion' -e 'left:' -e 'right:' "$WORK/out" | head -n 16 >&2
    exit 1
done
echo "107: $runs runs of the 4 tests that run a fake systemctl, 16 threads: every run passed"
