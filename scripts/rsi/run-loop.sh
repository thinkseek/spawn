#!/usr/bin/env bash
# Keep `spawn loop` alive.
#
# `spawn loop` runs the fleet: Bridge, Observer and the supervisor's liveness
# rule. It already retries transient store errors; this script covers a crash.
# The fleet never stops on its own, so every exit is retried with a growing
# pause. The RSI controller runs separately (`spawn rsi-loop`), and its halt
# stops only RSI.
#
# Configuration is the same as `spawn loop`: SPAWN_DATABASE_URL, SPAWN_GIT_REPO,
# SPAWN_WORKSPACE_ROOT.
set -u

PROGRAM="${SPAWN_PROGRAM:-spawn}"
pause=5

while :; do
  "$PROGRAM" loop "$@"
  code=$?
  echo "run-loop: spawn loop exited $code, restarting in ${pause}s" >&2
  sleep "$pause"
  # ponytail: backoff only, no restart cap. A cap turns a transient outage
  # into a permanent stop, which is the failure this script exists to avoid.
  pause=$((pause * 2))
  [ "$pause" -gt 60 ] && pause=60
done
