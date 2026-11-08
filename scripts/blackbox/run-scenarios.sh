#!/usr/bin/env bash
# Run every black-box scenario against one built binary.
#
# usage: SPAWN_PSQL=... SPAWN_PG_BASE=... scripts/blackbox/run-scenarios.sh [pattern]
#
# SPAWN_PSQL     command prefix of psql, without -d, e.g. `docker exec c psql -U postgres`
# SPAWN_PG_BASE  server URL without a database, e.g. postgres://postgres:spawn@127.0.0.1:15432
# SPAWN_BIN      binary under test; defaults to the cargo target directory
# SPAWN_TIMING   `adversarial` runs each scenario under perturbed timing
#                (scripts/timing/timing.sh: SPAWN_TIMING_SEED, _KINDS, _SCALE);
#                SPAWN_TIMING_OUT keeps each scenario's perturbation log there
#
# Every scenario gets its own database, its own bare repository, its own
# workspace root and its own tmux socket, so no scenario can see another.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
# shellcheck source=scripts/stage.sh
. "$root/scripts/stage.sh"
bin=${SPAWN_BIN:-${CARGO_TARGET_DIR:-$root/target}/debug/spawn}
pattern=${1:-*.sh}

if [ ! -x "$bin" ]; then
    echo "blackbox: no binary at $bin; build it first" >&2
    exit 1
fi
: "${SPAWN_PSQL:?SPAWN_PSQL is required}"
: "${SPAWN_PG_BASE:?SPAWN_PG_BASE is required}"

# Per-run suffix for databases and sockets. Not $$ alone: the gate runs this
# in a new PID namespace, where $$ is the same small number every time.
run="$$_${RANDOM}${RANDOM}"

passed=0
failed=0
matched=0
failed_names=""

# A scenario's database, and the restricted logins its init-db made: they are
# the cluster's, named after the database (spawn_login_role). One way for
# every scenario, here and in rsi/scenarios: scripts/blackbox/scenario-db.sh.
drop_database() {
    sh "$root/scripts/blackbox/scenario-db.sh" drop "$1" >/dev/null 2>&1
    return 0
}

cleanup() {
    [ -n "${work:-}" ] && rm -rf "$work"
    if [ -n "${db:-}" ]; then
        drop_database "$db"
    fi
    # Only a socket this runner made: an inherited SPAWN_TMUX_SOCKET may be
    # a live fleet's.
    if [ -n "${own_socket:-}" ]; then
        tmux -L "$own_socket" kill-server >/dev/null 2>&1
    fi
    return 0
}
trap cleanup EXIT

base_path=$PATH
if [ "${SPAWN_TIMING:-}" = adversarial ]; then
    # shellcheck source=scripts/timing/timing.sh
    . "$root/scripts/timing/timing.sh"
fi

for scenario in "$root"/scripts/blackbox/scenarios/$pattern; do
    [ -f "$scenario" ] || continue
    matched=$((matched + 1))
    slug=$(basename "$scenario" .sh)
    db="bb_${slug//[^a-zA-Z0-9]/_}_$run"
    work=$(mktemp -d)
    # Logins of this scenario's databases that a runner killed before its
    # cleanup left behind: this prefix only.
    sh "$root/scripts/blackbox/scenario-db.sh" sweep "${db%"$run"}" >/dev/null 2>&1

    if ! $SPAWN_PSQL -d postgres -tA -c "create database $db" >/dev/null 2>&1; then
        stage_emit blackbox fail 0 "$slug (create database)"
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
    SPAWN_TMUX_SOCKET="bb-$slug-$run"
    own_socket=$SPAWN_TMUX_SOCKET
    SPAWN_WORKER=worker-bb
    WORK=$work
    export SPAWN_BIN SPAWN_DATABASE_URL SPAWN_DB_NAME SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT \
        SPAWN_CLAUDE_PROGRAM SPAWN_TMUX_SOCKET SPAWN_WORKER WORK

    PATH=$base_path
    timing_out=""
    if [ "${SPAWN_TIMING:-}" = adversarial ]; then
        timing_out=${SPAWN_TIMING_OUT:-$work}/timing-$slug
        if ! timing_enable "$timing_out" "$bin"; then
            stage_emit blackbox fail 0 "$slug (timing)"
            failed=$((failed + 1))
            failed_names="$failed_names $slug"
            drop_database "$db"
            rm -rf "$work"
            continue
        fi
        SPAWN_BIN=$TIMING_SPAWN
        export SPAWN_BIN PATH
    fi

    stage_open blackbox "$slug"
    if (
        . "$root/scripts/blackbox/assertions.sh"
        . "$scenario"
    ); then
        stage_close ok
        passed=$((passed + 1))
    else
        stage_close fail
        failed=$((failed + 1))
        failed_names="$failed_names $slug"
    fi

    [ -n "$timing_out" ] && timing_report "$timing_out"
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    drop_database "$db"
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
