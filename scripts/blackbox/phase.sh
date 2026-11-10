# The preamble every black-box scenario (rsi/scenarios/0xx.sh) repeated:
# its exit contract, its own database and tmux socket, its cleanup, and the
# assertions. Sourced right after `set -eu`, with:
#
#   PHASE=011             the scenario's number: its messages' prefix, and
#                         its database's and socket's names
#   PHASE_DB=covered      the database name's word (bb_<PHASE>_<word>_<run>)
#   PHASE_TOOLS="..."     programs it needs beyond psql tmux git jq od bash sed
#   PHASE_LOOP_TAIL=15    lines of $WORK/loop.log shown on a failure (10)
#
# A scenario may define, before or after sourcing (they are called at exit):
#   phase_cleanup         its own teardown, run first (a loop wrapper, relays)
#   phase_diagnose        what to print on a failure, beside the loop's tail
#
# Exit contract, the gate's (scripts/rsi/gate.sh): 0 every check passed; 1
# an assertion failed (`fail`, which sets `asserted`); 2 a harness or
# infrastructure failure (`harness`, or any unexpected exit). INT and TERM
# exit 130: cleanup runs on the way out.
#
# It never puts a database URL on a command line: libpq reads the test
# server from the environment pg_url_export sets (scripts/pg-url.sh).

asserted=""
harness() {
    echo "$PHASE: harness: $*" >&2
    exit 2
}

[ -n "${PHASE-}" ] || { echo "phase.sh: PHASE is not set" >&2; exit 2; }
[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql tmux git jq od bash sed ${PHASE_TOOLS-}; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_${PHASE}_${PHASE_DB:?PHASE_DB is not set}_$run"
SPAWN_TMUX_SOCKET="bb-$PHASE-$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_TMUX_SOCKET SPAWN_PSQL

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
loop_pid=""
phase_teardown() {
    if command -v phase_cleanup >/dev/null 2>&1; then phase_cleanup || true; fi
    if [ -n "$loop_pid" ]; then
        kill "$loop_pid" 2>/dev/null || true
        wait "$loop_pid" 2>/dev/null || true
    fi
    tmux -L "$SPAWN_TMUX_SOCKET" kill-server >/dev/null 2>&1 || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$SPAWN_TMUX_SOCKET"
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    chmod -R u+rwX "$WORK" 2>/dev/null || true
    rm -rf "$WORK"
}
phase_finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ]; then
        if [ -s "$WORK/loop.log" ]; then
            echo "$PHASE: the loop's last lines:" >&2
            tail -n "${PHASE_LOOP_TAIL:-10}" "$WORK/loop.log" >&2
        fi
        if command -v phase_diagnose >/dev/null 2>&1; then phase_diagnose >&2 2>/dev/null || true; fi
    fi
    phase_teardown
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "$PHASE: harness: unexpected exit $rc" >&2
    exit 2
}
trap phase_finish EXIT
trap 'exit 130' INT TERM

# Logins of this scenario's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# An assertion exits 1, unless the database is gone: then nothing the
# scenario read can be trusted, and that is the harness's failure.
fail() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    asserted=1
    echo "blackbox: $*" >&2
    exit 1
}
