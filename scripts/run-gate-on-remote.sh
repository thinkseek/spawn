#!/usr/bin/env bash
# Sync the local Spawn tree to digest and run the remote gate.
# Usage: scripts/run-gate-on-remote.sh
set -euo pipefail

HOST="${SPAWN_GATE_HOST:-digest}"
REMOTE_DIR="${SPAWN_GATE_REMOTE_DIR:-/data00/home/chenkailun.c/spawn-workspace}"

ssh "$HOST" "mkdir -p '$REMOTE_DIR'"
rsync -az --delete \
  --exclude '.git' --exclude 'target' \
  ./ "$HOST:$REMOTE_DIR/"
ssh "$HOST" "bash '$REMOTE_DIR/scripts/run-gate.sh'"
