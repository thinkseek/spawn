#!/bin/sh
# Shared helpers for black-box scenarios.
#
# A scenario is a POSIX shell script that drives the `spawn` binary and reads
# only what an outside observer can see: exit codes, standard output, the
# database, the workspace directory and the terminal pane.
#
# The runner exports, for every scenario:
#
#   SPAWN_BIN               the binary under test
#   SPAWN_DATABASE_URL      a database of this scenario alone
#   SPAWN_DB_NAME           its name, for `sql`
#   SPAWN_PSQL              command prefix of psql, e.g. `docker exec c psql -U postgres`
#   SPAWN_GIT_REPO          a bare repository holding one `main` commit
#   SPAWN_WORKSPACE_ROOT    an empty directory
#   SPAWN_CLAUDE_PROGRAM    a fake engine that echoes each typed line
#   SPAWN_TMUX_SOCKET       a private tmux socket
#   WORK                    scratch directory of this scenario
#
# Transient output of `capture` lands in `$WORK/out.txt` and `$WORK/err.txt`.

set -eu

fail() {
    echo "blackbox: $*" >&2
    exit 1
}

assert_eq() {
    [ "$1" = "$2" ] || fail "${3:-value}: expected [$2], got [$1]"
}

assert_contains() {
    case "$1" in
        *"$2"*) ;;
        *) fail "${3:-text}: expected to contain [$2], got [$1]" ;;
    esac
}

assert_not_contains() {
    case "$1" in
        *"$2"*) fail "${3:-text}: must not contain [$2]" ;;
    esac
}

assert_file() { [ -f "$1" ] || fail "missing file $1"; }
assert_no_file() { [ ! -e "$1" ] || fail "unexpected path $1"; }

# One SQL statement, tuples only.
sql() { $SPAWN_PSQL -d "$SPAWN_DB_NAME" -tA -c "$1"; }

# One SQL statement reduced to its first line, without its trailing newline.
# Internal spaces are kept: a scalar may be a prompt, not only a number.
scalar() { sql "$1" | head -1; }

# Run a command and record its exit code, without tripping `set -e`.
capture() {
    status=0
    "$@" >"$WORK/out.txt" 2>"$WORK/err.txt" || status=$?
    return 0
}

out() { cat "$WORK/out.txt"; }
err() { cat "$WORK/err.txt"; }

# Poll a command until it succeeds, up to `limit` quarter seconds.
wait_for() {
    limit=$1
    shift
    n=0
    while [ "$n" -lt "$limit" ]; do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        sleep 0.25
        n=$((n + 1))
    done
    return 1
}

# Last lines of the only terminal of this scenario.
pane() {
    tmux -L "$SPAWN_TMUX_SOCKET" capture-pane -p -t "=$(terminal):" -S -80
}

terminal() {
    tmux -L "$SPAWN_TMUX_SOCKET" list-sessions -F '#{session_name}' | head -1
}
