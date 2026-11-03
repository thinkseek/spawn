#!/usr/bin/env bash
# Keep `spawn loop` alive.
#
# `spawn loop` already retries transient store errors. This script only covers
# a crash. Exit code 2 is the RSI halt (three rejections in a row): that is a
# decision for a human, so the supervisor stops instead of restarting. Every
# other exit is retried with a growing pause.
#
# Configuration is the same as `spawn loop`: SPAWN_DATABASE_URL, SPAWN_GIT_REPO,
# SPAWN_WORKSPACE_ROOT and the optional SPAWN_RSI_* paths.
set -u

PROGRAM="${SPAWN_PROGRAM:-spawn}"
pause=5

while :; do
  "$PROGRAM" loop "$@"
  code=$?
  if [ "$code" -eq 2 ]; then
    echo "run-loop: RSI halted, stopping" >&2
    exit 2
  fi
  echo "run-loop: spawn loop exited $code, restarting in ${pause}s" >&2
  sleep "$pause"
  # ponytail: backoff only, no restart cap. A cap turns a transient outage
  # into a permanent stop, which is the failure this script exists to avoid.
  pause=$((pause * 2))
  [ "$pause" -gt 60 ] && pause=60
done
