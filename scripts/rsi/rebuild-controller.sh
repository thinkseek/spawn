#!/bin/sh
# Build the controller binary from the current baseline.
#
# usage: SPAWN_GIT_REPO=... SPAWN_GIT_ROOT=... scripts/rsi/rebuild-controller.sh
#
# Prints the binary path on the last line. The next `spawn evolve` and
# `spawn tick` must run that binary, so the loop executes the code it landed.
#
# ponytail: a changed baseline costs one full build. Restore a cached target
# directory here when the cost matters.
set -eu

repo=${SPAWN_GIT_REPO:?SPAWN_GIT_REPO is required}
build=${SPAWN_RSI_BUILD:?SPAWN_RSI_BUILD is required, e.g. $HOME/.cache/spawn-build}

rm -rf "$build"
mkdir -p "$build"
git --git-dir="$repo" archive refs/heads/main | tar -x -C "$build"
(cd "$build" && cargo build -q --bin spawn)
echo "$build/target/debug/spawn"
