#!/usr/bin/env bash
# End to end smoke test: real PostgreSQL, real git, real tmux and a fake
# Claude screen. Verifies that a submitted command reaches a terminal, that
# Claude runs with --ax-screen-reader, that the workspace was extracted from
# the base commit, that the pane tail decides Ready, that the prompt is typed,
# and that an observation is recorded.
#
# Usage: SPAWN_DATABASE_URL=... scripts/e2e-smoke.sh
# The binary defaults to $CARGO_TARGET_DIR/debug/spawn (else target/debug/spawn)
# and can be set with SPAWN_BIN.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
bin=${SPAWN_BIN:-}
if [ -z "$bin" ]; then
  (cd "$root" && cargo build --quiet -p spawn-cli)
  bin=${CARGO_TARGET_DIR:-$root/target}/debug/spawn
fi
url=${SPAWN_DATABASE_URL:?SPAWN_DATABASE_URL is required}
psql=${SPAWN_PSQL:-}
tmux=${SPAWN_TMUX_PROGRAM:-tmux}
work=$(mktemp -d)
socket="spawn-e2e-$$"
key="e2e-$$"
agent="agent_e2e"
session="session_e2e_$$"
repo=$work/repo.git
workspaces=$work/ws

cleanup() {
  "$tmux" -L "$socket" kill-server 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "e2e: $*" >&2
  exit 1
}

# A bare repository with one base commit, and an empty workspace root.
git init -q --bare --initial-branch=main "$repo"
seed=$work/seed
git init -q --initial-branch=main "$seed"
printf 'base\n' >"$seed/README.md"
git -C "$seed" add -A
git -C "$seed" -c user.name=e2e -c user.email=e2e@localhost commit -qm base
git -C "$seed" push -q "$repo" main:main
mkdir -p "$workspaces"

# Fake Claude: records its argv, draws a screen the tail parser reads as Ready,
# then echoes every typed line and waits again.
cat >"$work/claude" <<CLAUDE
#!/bin/sh
printf '%s\n' "\$*" >"$work/argv.log"
printf 'Welcome to Claude Code\n'
printf '\n'
printf '❯ '
while IFS= read -r line; do
  printf '\n%s\n' "echo: \$line"
  printf '❯ '
done
CLAUDE

chmod 755 "$work/claude"

export SPAWN_DATABASE_URL=$url
export SPAWN_GIT_REPO=$repo
export SPAWN_WORKSPACE_ROOT=$workspaces
export SPAWN_CLAUDE_PROGRAM=$work/claude
export SPAWN_TMUX_PROGRAM=$tmux
export SPAWN_TMUX_SOCKET=$socket
export SPAWN_WORKER=worker-e2e
export SPAWN_TRACE=1
trace=$work/trace.log

# Wall time of one step in milliseconds, appended to the trace as e2e.<name>.
now_us() { echo "${EPOCHREALTIME/./}"; }
step() {
  name=$1; shift
  t0=$(now_us)
  if ! "$@" 2>>"$trace"; then
    grep -v '^spawn-trace' "$trace" >&2 || true
    fail "step $name failed"
  fi
  echo "spawn-trace e2e.$name $(( $(now_us) - t0 ))" >>"$trace"
}

step init_db "$bin" init-db >/dev/null
# A fresh database works too; reset only when psql is reachable.
if [ -n "$psql" ]; then
  $psql "delete from spawn_commands; delete from spawn_events; delete from spawn_session_state;" >/dev/null
fi

command=$(step submit "$bin" submit --principal spawn --agent "$agent" --session "$session" \
  --key "$key" \
  --action '{"type":"start","engine":"claude","prompt":"say hello"}')
echo "e2e: $command"

t_submit=$(now_us)
step tick "$bin" tick --repo "$repo" --root "$workspaces"

# tmux starts the engine asynchronously, so its argv appears shortly after
# tick returns.
ran=0
for _ in $(seq 1 40); do
  if grep -q -- '--ax-screen-reader' "$work/argv.log" 2>/dev/null; then
    ran=1
    break
  fi
  sleep 0.25
done
[ "$ran" = 1 ] || fail "Claude did not run with --ax-screen-reader"
[ -f "$workspaces/$agent/README.md" ] ||
  fail "the base tree was not extracted into the workspace"
[ ! -e "$workspaces/$agent/.git" ] ||
  fail "the engine must not see the repository"

terminal=$("$tmux" -L "$socket" list-sessions -F '#{session_name}' | head -1)
[ -n "$terminal" ] || fail "no tmux session"
# The engine echoes the typed prompt asynchronously; poll the tail.
found=0
for _ in $(seq 1 40); do
  "$tmux" -L "$socket" capture-pane -p -t "=$terminal:" -S -80 >"$work/pane.txt"
  if grep -q 'echo: say hello' "$work/pane.txt"; then
    found=1
    echo "spawn-trace e2e.submit_to_echo $(( $(now_us) - t_submit ))" >>"$trace"
    break
  fi
  sleep 0.25
done
if [ "$found" = 0 ]; then
  cat "$work/pane.txt" >&2
  fail "the prompt did not reach the pane"
fi

step status "$bin" status >"$work/status.txt"
grep -q "$session" "$work/status.txt" ||
  fail "no observation for $session"
echo "e2e: ok ($terminal)"

grep -v '^spawn-trace' "$trace" >&2 || true

# Per-stage timing: calls, total and max in milliseconds.
echo "e2e: timing (stage calls total_ms max_ms)"
awk '$1 == "spawn-trace" {
  n[$2]++; t[$2] += $3; if ($3 > m[$2]) m[$2] = $3
} END {
  for (k in n) printf "  %-22s %4d %9.2f %9.2f\n", k, n[k], t[k] / 1000, m[k] / 1000
}' "$trace" | sort
if [ -n "${SPAWN_TRACE_OUT:-}" ]; then cp "$trace" "$SPAWN_TRACE_OUT"; fi
