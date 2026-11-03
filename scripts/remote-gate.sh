#!/usr/bin/env bash
# Sync the local Spawn tree to digest and run the remote gate.
# Usage: scripts/remote-gate.sh
set -euo pipefail

HOST="digest"
REMOTE_DIR="/data00/home/chenkailun.c/spawn-workspace"

ssh "$HOST" "mkdir -p '$REMOTE_DIR'"
rsync -az --delete \
  --exclude '.git' --exclude 'target' --exclude 'gate-logs' \
  ./ "$HOST:$REMOTE_DIR/"
ssh "$HOST" "bash '$REMOTE_DIR/scripts/gate-on-digest.sh'"
