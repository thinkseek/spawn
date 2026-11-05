#!/usr/bin/env bash
# The e2e's waits on a resident's task states (scripts/e2e.sh, section 7),
# run on a stub board instead of a fleet: no database, no loop, a few
# seconds.
#
# A resident's task goes delivered -> reported by itself as soon as the
# worker reports, which can fall between two of the e2e's polls. A wait for
# exactly `delivered` then never succeeds (task 36 was rejected by the gate
# with "timed out waiting for task 1 to be delivered"). This lifts the real
# lines from the e2e (its helpers, the waits for "task $t1 to be delivered"
# and "task $t2 to be delivered after $t1", and the one-at-a-time check
# between them) and checks:
#   (1) a task already reported at the first poll satisfies the delivered
#       wait, and the second task still assigned passes the check;
#   (2) the second task delivered too still fails the one-at-a-time check;
#   (3) a task still assigned does not satisfy the wait (it times out);
#   (4) the second task's wait accepts it reported at the first poll.
#
# Usage: scripts/test-e2e-waits.sh [path/to/e2e.sh]   (default: this tree's)
set -euo pipefail

e2e=${1:-$(cd "$(dirname "$0")" && pwd)/e2e.sh}
[ -r "$e2e" ] || { echo "test-e2e-waits: no $e2e" >&2; exit 2; }
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# lift <fixed string>: the one line of the e2e that holds it, its wait cut
# to 2 s.
lift() {
  local lines
  lines=$(grep -F -- "$1" "$e2e") || { echo "test-e2e-waits: no line with $1 in $e2e" >&2; exit 2; }
  [ "$(printf '%s\n' "$lines" | wc -l)" -eq 1 ] || { echo "test-e2e-waits: $1 is on several lines" >&2; exit 2; }
  printf '%s\n' "$lines" | sed -E 's/^wait_for [0-9]+ /wait_for 2 /'
}
{
  sed -n '/^wait_for() {$/,/^}$/p' "$e2e"
  grep -E '^(task_state|state_is)\(\) ' "$e2e"
  sed -n '/^state_in() {$/,/^}$/p' "$e2e"
} >"$work/helpers.sh"
grep -q '^wait_for() {' "$work/helpers.sh" && grep -q '^task_state() ' "$work/helpers.sh" ||
  { echo "test-e2e-waits: the e2e's helpers were not found in $e2e" >&2; exit 2; }
# shellcheck disable=SC2016 # the e2e's own lines, expanded when they run
{
  printf 'first_wait() {\n%s\n}\n' "$(lift 'wait_for 60 "task $t1 to be delivered"')"
  printf 'one_at_a_time() {\n%s\n}\n' "$(lift 'fail "the resident got two tasks at once"')"
  printf 'second_wait() {\n%s\n}\n' "$(lift '"task $t2 to be delivered after $t1"')"
} >"$work/lines.sh"

# case <name> <state of task 1> <state of task 2> <steps> <expected>: run the
# lifted steps on a board that shows those states, expecting `ok` or the
# message `fail` was given.
failures=0
check() {
  local name=$1 s1=$2 s2=$3 steps=$4 expected=$5 got
  got=$(
    t1=1 t2=2
    board() {
      printf '{"missions":[{"tasks":[{"id":1,"state":"%s"},{"id":2,"state":"%s"}]}]}\n' "$s1" "$s2"
    }
    fail() { echo "$*"; exit 1; }
    # shellcheck source=/dev/null
    . "$work/helpers.sh"
    # shellcheck source=/dev/null
    . "$work/lines.sh"
    for step in $steps; do "$step"; done
    echo ok
  ) || true
  if [ "$got" = "$expected" ]; then
    echo "ok   $name"
  else
    echo "FAIL $name: expected '$expected', got '$got'"
    failures=$((failures + 1))
  fi
}

check "(1) task 1 already reported at the first poll" reported assigned "first_wait one_at_a_time" ok
check "(1) task 1 delivered at the first poll" delivered assigned "first_wait one_at_a_time" ok
check "(2) two tasks delivered at once" delivered delivered "first_wait one_at_a_time" \
  "the resident got two tasks at once"
check "(3) task 1 never delivered" assigned assigned "first_wait" \
  'timed out waiting for task 1 to be delivered'
check "(4) task 2 already reported at the first poll" accepted reported "second_wait" ok

[ "$failures" -eq 0 ] || { echo "test-e2e-waits: $failures failed ($e2e)"; exit 1; }
echo "test-e2e-waits: PASS ($e2e)"
