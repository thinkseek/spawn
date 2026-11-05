#!/usr/bin/env bash
# deploy/controller.sh against a scratch live directory and a scratch repo;
# nothing live is touched.
#
#   deploy/test-controller.sh
#
# Covered: a build is the commit's tree, read-only, with web/node_modules
# copied in (and refused when installed from another lockfile); the switch
# is atomic (a reader resolving the symlink in a loop never misses, while
# the same loop over unlink-then-symlink does); a process holding the previous
# controller (a running script, a working directory, an open file) keeps
# reading that commit's files across two switches, and its controller is
# kept while it runs and removed at the next switch after; only the current
# and previous controllers remain otherwise; a leftover half build is
# removed; build and switch run again change nothing.
#
# TEST_CONTROLLER_SCRIPT=<path> runs that script instead of the checkout's.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
script=${TEST_CONTROLLER_SCRIPT:-$repo/deploy/controller.sh}

failures=0
pass() { printf 'ok   %s\n' "$*"; }
fail() { printf 'FAIL %s\n' "$*"; failures=$((failures + 1)); }
check() { # check <description> <command...>
  local what=$1
  shift
  if "$@"; then pass "$what"; else fail "$what"; fi
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-controller-test.XXXXXX")
holders=()
cleanup() {
  for pid in "${holders[@]}"; do kill "$pid" 2>/dev/null || true; done
  chmod -R u+w "$scratch" 2>/dev/null || true
  rm -rf "$scratch"
}
trap cleanup EXIT

export SPAWN_LIVE=$scratch/live
mkdir -p "$SPAWN_LIVE"
dir=$SPAWN_LIVE/controllers
ctl() { bash "$script" "$@"; }

# The scratch repo: commits A..D, each naming itself in rsi/scenarios/which
# and in a script that sleeps first (a gate in the middle of its run).
src=$scratch/src
git init -q "$src"
commit() {
  mkdir -p "$src/rsi/scenarios" "$src/scripts" "$src/web"
  echo "$1" >"$src/rsi/scenarios/which"
  printf '#!/bin/sh\nwhile [ ! -e "$1" ]; do sleep 0.1; done\necho "ran %s"\ncat "$(dirname "$0")/../rsi/scenarios/which"\n' "$1" >"$src/scripts/gate.sh"
  echo "lockfile v1" >"$src/web/pnpm-lock.yaml"
  git -C "$src" add -A
  git -C "$src" -c user.name=t -c user.email=t@t commit -q -m "$1"
  git -C "$src" rev-parse HEAD
}
A=$(commit A)
B=$(commit B)
C=$(commit C)
D=$(commit D)
git_dir=$src/.git

modules=$scratch/node_modules
mkdir -p "$modules/.pnpm" "$modules/.bin"
echo "lockfile v1" >"$modules/.pnpm/lock.yaml"
echo "tool v1" >"$modules/.bin/tool"

echo "== build"
out=$(ctl build spawn "$git_dir" "$A" "$modules" 2>"$scratch/err")
check "prints the directory" [ "$out" = "$dir/spawn-$A" ]
check "holds the commit's tree" [ "$(cat "$dir/spawn-$A/rsi/scenarios/which")" = A ]
check "names its commit" [ "$(cat "$dir/spawn-$A/.controller")" = "$A" ]
check "web/node_modules is copied in" [ "$(cat "$dir/spawn-$A/web/node_modules/.bin/tool")" = "tool v1" ]
check "the copy is not the source's inodes (reflink or copy here)" \
  [ "$(stat -c %i "$modules/.bin/tool")" != "$(stat -c %i "$dir/spawn-$A/web/node_modules/.bin/tool")" ]
echo "tool v2" >"$modules/.bin/tool.new" && mv "$modules/.bin/tool.new" "$modules/.bin/tool"
check "a reinstall in the source does not reach it" [ "$(cat "$dir/spawn-$A/web/node_modules/.bin/tool")" = "tool v1" ]
check "a file of the tree is read-only" bash -c '! echo x 2>/dev/null >"$1"' _ "$dir/spawn-$A/rsi/scenarios/which"
check "no file can be added to the tree" bash -c '! touch "$1" 2>/dev/null' _ "$dir/spawn-$A/rsi/scenarios/new.sh"
check "nothing is current before a switch" [ ! -e "$dir/spawn" ]
echo "lockfile v2" >"$modules/.pnpm/lock.yaml"
set +e
ctl build spawn "$git_dir" "$B" "$modules" >/dev/null 2>"$scratch/err"
status=$?
set -e
check "a node_modules installed from another lockfile is refused" [ "$status" -ne 0 ]
check "says so" grep -q "installed from another lockfile" "$scratch/err"
check "and leaves nothing behind" [ -z "$(find "$dir" -maxdepth 1 -name "*$B*")" ]
echo "lockfile v1" >"$modules/.pnpm/lock.yaml"

echo "== switch"
ctl switch spawn "$A" 2>/dev/null
check "spawn -> spawn-$A" [ "$(readlink "$dir/spawn")" = "spawn-$A" ]
check "the path through it is A's" [ "$(cat "$dir/spawn/rsi/scenarios/which")" = A ]
set +e
ctl switch spawn "$B" 2>"$scratch/err"
status=$?
set -e
check "a commit not built is refused" [ "$status" -ne 0 ]
check "and the controller stays" [ "$(readlink "$dir/spawn")" = "spawn-$A" ]

echo "== the switch is atomic"
ctl build spawn "$git_dir" "$B" "$modules" >/dev/null 2>&1
# reader <file>: resolve the symlink as fast as it can until <file> exists;
# prints how many reads found nothing.
reader() {
  local misses=0
  while [ ! -e "$1" ]; do
    { read -r _ <"$dir/spawn/rsi/scenarios/which"; } 2>/dev/null || misses=$((misses + 1))
  done
  echo "$misses"
}
reader "$scratch/stop-1" >"$scratch/misses-1" &
for _ in $(seq 100); do
  ctl switch spawn "$B" 2>/dev/null
  ctl switch spawn "$A" 2>/dev/null
done
touch "$scratch/stop-1"
wait
check "200 switches under a reader: it never missed (missed $(cat "$scratch/misses-1"))" \
  [ "$(cat "$scratch/misses-1")" = 0 ]
# The same reader over a replace that is not atomic (unlink, then symlink:
# what `ln -sfn` does outside recent GNU coreutils): this test sees a miss.
reader "$scratch/stop-2" >"$scratch/misses-2" &
for _ in $(seq 2000); do
  rm -f "$dir/spawn" && ln -s "spawn-$B" "$dir/spawn"
  rm -f "$dir/spawn" && ln -s "spawn-$A" "$dir/spawn"
done
touch "$scratch/stop-2"
wait
check "the same reader over unlink-then-symlink misses (missed $(cat "$scratch/misses-2")): the check above means something" \
  [ "$(cat "$scratch/misses-2")" -gt 0 ]
ctl switch spawn "$A" 2>/dev/null

echo "== a gate holding the previous controller"
# Three holders of A, each started through the symlink and pinned as the
# integrator and the gate pin it: a script being run (sh keeps it open), a
# working directory, an open file.
sh "$(readlink -f "$dir/spawn/scripts/gate.sh")" "$scratch/go" >"$scratch/gate.out" 2>&1 &
gate=$!
holders+=("$gate")
(cd -P "$dir/spawn/rsi/scenarios" && while [ ! -e "$scratch/go" ]; do sleep 0.1; done && cat which >"$scratch/cwd.out") &
holders+=($!)
held_cwd=$!
(exec 3<"$dir/spawn/rsi/scenarios/which" && while [ ! -e "$scratch/go" ]; do sleep 0.1; done && cat <&3 >"$scratch/fd.out") &
holders+=($!)
held_fd=$!
sleep 0.5
ctl update spawn "$git_dir" "$C" "$modules" 2>/dev/null >/dev/null
ctl update spawn "$git_dir" "$D" "$modules" 2>"$scratch/err" >/dev/null
check "two switches later spawn is D" [ "$(cat "$dir/spawn/rsi/scenarios/which")" = D ]
check "spawn.prev is C" [ "$(readlink "$dir/spawn.prev")" = "spawn-$C" ]
check "A, neither current nor previous, is kept: it is in use" [ -d "$dir/spawn-$A" ]
check "says so" grep -q "kept spawn-$A: a process still uses it" "$scratch/err"
check "B, which nothing uses, is removed" [ ! -e "$dir/spawn-$B" ]
touch "$scratch/go"
wait "$gate" "$held_cwd" "$held_fd"
holders=()
check "the running script finished as A's: $(tr '\n' ' ' <"$scratch/gate.out")" \
  [ "$(cat "$scratch/gate.out")" = "$(printf 'ran A\nA')" ]
check "the working directory still read A's scenarios" [ "$(cat "$scratch/cwd.out")" = A ]
check "the open file still read A's" [ "$(cat "$scratch/fd.out")" = A ]

echo "== the cleanup"
mkdir -p "$dir/.spawn-$B.tmp.XXXXXX/rsi" # a build killed halfway
ctl update spawn "$git_dir" "$C" "$modules" 2>"$scratch/err" >/dev/null
check "back to C, D is the previous" \
  bash -c '[ "$(readlink "$1/spawn")" = "spawn-$2" ] && [ "$(readlink "$1/spawn.prev")" = "spawn-$3" ]' _ "$dir" "$C" "$D"
check "A, no longer used, is removed" [ ! -e "$dir/spawn-$A" ]
check "the half build is removed" [ ! -e "$dir/.spawn-$B.tmp.XXXXXX" ]
check "only the current and the previous remain" \
  [ "$(cd "$dir" && find . -mindepth 1 -maxdepth 1 -name 'spawn-*' | sort | tr '\n' ' ')" = "$(printf './spawn-%s ./spawn-%s ' "$C" "$D" | tr ' ' '\n' | sort | grep . | tr '\n' ' ')" ]

echo "== run again"
before=$(cd "$dir" && ls -la --time-style=+%s.%N)
ctl update spawn "$git_dir" "$C" "$modules" 2>"$scratch/err" >/dev/null
check "says it is built and current" \
  bash -c 'grep -q "spawn-$2 is already built" "$1" && grep -q "spawn is already spawn-$2" "$1"' _ "$scratch/err" "$C"
check "nothing changed" [ "$before" = "$(cd "$dir" && ls -la --time-style=+%s.%N)" ]

echo "== another name beside it"
ctl update supply-pa "$git_dir" "$B" 2>/dev/null >/dev/null
check "supply-pa is B, with no node_modules" \
  bash -c '[ "$(cat "$1/supply-pa/rsi/scenarios/which")" = B ] && [ ! -e "$1/supply-pa/web/node_modules" ]' _ "$dir"
check "spawn is untouched" [ "$(readlink "$dir/spawn")" = "spawn-$C" ]

echo
if [ "$failures" -eq 0 ]; then echo "test-controller: all passed"; else
  echo "test-controller: $failures failed"
  exit 1
fi
