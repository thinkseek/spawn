#!/usr/bin/env bash
# Sync the local Spawn tree to digest and run the remote gate.
# Usage: scripts/run-gate-on-remote.sh
set -euo pipefail

HOST="${SPAWN_GATE_HOST:-digest}"
REMOTE_DIR="${SPAWN_GATE_REMOTE_DIR:-/data00/home/chenkailun.c/spawn-workspace}"

# Two gates share one remote directory, and rsync --delete makes a second run
# destroy the tree the first one is building. One gate at a time.
LOCK_DIR="${TMPDIR:-/tmp}/spawn-gate.lock"
if ! mkdir "$LOCK_DIR" 2>/dev/null; then
  holder="$(cat "$LOCK_DIR/pid" 2>/dev/null || true)"
  if [ -n "$holder" ] && kill -0 "$holder" 2>/dev/null; then
    echo "run-gate-on-remote: another gate runs as pid $holder" >&2
    exit 3
  fi
  rm -rf "$LOCK_DIR"
  mkdir "$LOCK_DIR" 2>/dev/null || { echo "run-gate-on-remote: lock race" >&2; exit 3; }
fi
echo $$ > "$LOCK_DIR/pid"
trap 'rm -rf "$LOCK_DIR"' EXIT

ssh "$HOST" "mkdir -p '$REMOTE_DIR'"
rsync -az --delete \
  --exclude '.git' --exclude 'target' --exclude 'node_modules' \
  ./ "$HOST:$REMOTE_DIR/"
ssh "$HOST" "bash '$REMOTE_DIR/scripts/run-gate.sh'"
