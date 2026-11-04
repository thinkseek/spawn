#!/usr/bin/env bash
# Run every black-box scenario against one built binary.
#
# usage: SPAWN_PSQL=... SPAWN_PG_BASE=... scripts/blackbox/run-scenarios.sh [pattern]
#
# SPAWN_PSQL     command prefix of psql, without -d, e.g. `docker exec c psql -U postgres`
# SPAWN_PG_BASE  server URL without a database, e.g. postgres://postgres:PASSWORD@127.0.0.1:15432
# SPAWN_BIN      binary under test; defaults to the cargo target directory
#
# Every scenario gets its own database, its own bare repository, its own
# workspace root and its own tmux socket, so no scenario can see another.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
bin=${SPAWN_BIN:-${CARGO_TARGET_DIR:-$root/target}/debug/spawn}
pattern=${1:-*.sh}

if [ ! -x "$bin" ]; then
    echo "blackbox: no binary at $bin; build it first" >&2
    exit 1
fi
: "${SPAWN_PSQL:?SPAWN_PSQL is required}"
: "${SPAWN_PG_BASE:?SPAWN_PG_BASE is required}"

passed=0
failed=0
matched=0
failed_names=""

cleanup() {
    [ -n "${work:-}" ] && rm -rf "$work"
    if [ -n "${db:-}" ]; then
        $SPAWN_PSQL -d postgres -tA -c "drop database if exists $db with (force)" >/dev/null 2>&1
    fi
    if [ -n "${SPAWN_TMUX_SOCKET:-}" ]; then
        tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1
    fi
    return 0
}
trap cleanup EXIT

for scenario in "$root"/scripts/blackbox/scenarios/$pattern; do
    [ -f "$scenario" ] || continue
    matched=$((matched + 1))
    slug=$(basename "$scenario" .sh)
    db="bb_${slug//[^a-zA-Z0-9]/_}_$$"
    work=$(mktemp -d)

    if ! $SPAWN_PSQL -d postgres -tA -c "create database $db" >/dev/null 2>&1; then
        echo "FAIL $slug (create database)"
        failed=$((failed + 1))
        failed_names="$failed_names $slug"
        rm -rf "$work"
        continue
    fi

    repo=$work/repo.git
    seed=$work/seed
    git init -q --bare --initial-branch=main "$repo"
    git init -q --initial-branch=main "$seed"
    printf 'base\n' >"$seed/README.md"
    git -C "$seed" add -A
    git -C "$seed" -c user.name=bb -c user.email=bb@localhost commit -qm base
    git -C "$seed" push -q "$repo" main:main
    mkdir -p "$work/ws"

    cat >"$work/claude" <<'FAKE'
#!/bin/sh
printf 'Welcome to Claude Code\n\n'
printf '❯ '
while IFS= read -r line; do
  printf '\n%s\n' "echo: $line"
  printf '❯ '
done
FAKE
    chmod 755 "$work/claude"

    SPAWN_BIN=$bin
    SPAWN_DATABASE_URL="$SPAWN_PG_BASE/$db"
    SPAWN_DB_NAME=$db
    SPAWN_GIT_REPO=$repo
    SPAWN_WORKSPACE_ROOT=$work/ws
    SPAWN_CLAUDE_PROGRAM=$work/claude
    SPAWN_CODEX_PROGRAM=$work/claude
    SPAWN_TMUX_SOCKET="bb-$slug-$$"
    SPAWN_WORKER=worker-bb
    SPAWN_RSI_BACKLOG=$root/rsi/backlog
    SPAWN_RSI_SCENARIOS=$root/rsi/scenarios
    SPAWN_RSI_JUDGE=$root/scripts/rsi/gate.sh
    WORK=$work
    export SPAWN_BIN SPAWN_DATABASE_URL SPAWN_DB_NAME SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT \
        SPAWN_CLAUDE_PROGRAM SPAWN_CODEX_PROGRAM SPAWN_TMUX_SOCKET SPAWN_WORKER \
        SPAWN_RSI_BACKLOG SPAWN_RSI_SCENARIOS SPAWN_RSI_JUDGE WORK

    if (
        . "$root/scripts/blackbox/assertions.sh"
        . "$scenario"
    ); then
        echo "ok   $slug"
        passed=$((passed + 1))
    else
        echo "FAIL $slug"
        failed=$((failed + 1))
        failed_names="$failed_names $slug"
    fi

    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    $SPAWN_PSQL -d postgres -tA -c "drop database if exists $db with (force)" >/dev/null 2>&1 || true
    rm -rf "$work"
done

if [ "$matched" -eq 0 ]; then
    echo "blackbox: no scenario matches $pattern" >&2
    exit 1
fi

echo "blackbox: $passed passed, $failed failed"
if [ "$failed" -ne 0 ]; then
    echo "blackbox: failed:$failed_names"
    exit 1
fi
