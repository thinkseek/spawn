#!/bin/sh
# Black box: every alert ends with an outcome, the owner can judge one that
# should not have been raised, and the Core lines card says how often Needs
# you was right.
#
# The problem (owner, 2026-11-12 13:30; sync/roadmap-effects.md,
# Observability, north star "Needs you is right"): the observability line's
# north star is the share of the items Needs you showed that were true and
# really needed the owner. The record cannot tell: an alert is open or
# cleared, and a cleared alert looks the same whether its condition went
# away (it was right) or it was false, stale or a duplicate of another
# (it was wrong). By hand on 11-12: four false "message not taken" alerts
# in one day. Monitoring only: no gate, integrator or deploy reads any of it.
#
# Required (through `spawn alert judge`, `GET /api/alerts` and
# `GET /api/core-lines`):
#   (1) `spawn alert judge <key> false|stale|duplicate` (the owner's)
#       records `alert.judged` {key, verdict} through spawn_alert_record and
#       says so; an open alert it judges is cleared by it, its cleared
#       evidence {judged: <verdict>}. It refuses (exit not 0, nothing
#       written) another verdict, a key no alert has, a notice, and an alert
#       already judged.
#   (2) `GET /api/alerts`' rows carry `outcome`: null while open (and for a
#       notice), "cleared" when its condition cleared it, else its verdict.
#   (3) `GET /api/core-lines`' `observability.needs_you_right`: of the alerts
#       (not notices) that ended in the last 7 days, the share whose outcome
#       is "cleared"; null with none.
#
# The history: alerts as the loop records them (spawn_alert_record): a1 and
# a2 fired and cleared, a3 and a4 fired and open, a5 a notice. Then the
# owner judges a2 false and a3 a duplicate.
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure. Any unexpected
# exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "092: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
# The tree under test: the gate runs a scenario from the candidate's tree,
# and $controller is the deployed controller's copy, which lacks the
# candidate's web/src until it lands.
tree=$(pwd)
[ -f "$tree/web/package.json" ] && [ -d "$tree/web/src" ] ||
    harness "run from the tree under test: no web/src under $tree"
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od curl sed grep sha1sum cut node; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
# Gone again if a check below refuses, before the cleanup is set.
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Not $$ alone: the gate's PID namespace makes it the same small number.
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_092_outcomes_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
# A scratch HOME and Claude Code projects dir: nothing here reads or writes
# the real ones.
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
# The runtime directory too (backlog S20): what Spawn keeps there (a
# launch's environment file, its terminals' leaders) stays in WORK, never
# in the real one. systemd's user manager stays reachable by its bus.
real_runtime=${XDG_RUNTIME_DIR-}
if [ -z "${DBUS_SESSION_BUS_ADDRESS-}" ] && [ -S "$real_runtime/bus" ]; then
    DBUS_SESSION_BUS_ADDRESS=unix:path=$real_runtime/bus
    export DBUS_SESSION_BUS_ADDRESS
fi
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
serve_pid=""
cleanup() {
    if [ -n "$serve_pid" ]; then
        kill "$serve_pid" 2>/dev/null || true
        wait "$serve_pid" 2>/dev/null || true
    fi
    if [ -n "$database_made" ]; then
        # The database and the logins init-db made for it.
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    if [ "$rc" -ne 0 ] && [ -s "$WORK/api.json" ]; then
        echo "092: the last answer (first 60 lines):" >&2
        head -n 60 "$WORK/api.json" >&2
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "092: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

# Logins of this scenario's databases that a run killed before its cleanup
# left behind (scripts/blackbox/scenario-db.sh): this prefix only.
sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

# A failed check is reported and the run goes on; the run fails at its end
# if any did. Unless the database is gone: then nothing read can be trusted.
: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done


note() { echo "092: $*" >&2; }
expect_eq() { # <got> <want> <what>
    [ "$1" = "$2" ] || check_failed "$3: expected [$2], got [$1]"
}


# --- the history -------------------------------------------------------------------------

cat >"$WORK/seed.sql" <<'SQL'
\set ON_ERROR_STOP 1
select spawn_alert_record(jsonb_build_object('type', 'alert.fired', 'key', 'bb:a' || k, 'kind', 'bb_kind',
       'subject', 'bb subject ' || k, 'text', 'alert ' || k, 'evidence', '{}'::jsonb, 'notice', k = 5))
  from generate_series(1, 5) k;
select spawn_alert_record(jsonb_build_object('type', 'alert.cleared', 'key', 'bb:a' || k, 'kind', 'bb_kind',
       'subject', 'bb subject ' || k, 'evidence', '{"why": "the condition went away"}'::jsonb))
  from generate_series(1, 2) k;
SQL
capture psql -d "$SPAWN_DB_NAME" -q -f "$WORK/seed.sql"
[ "$status" -eq 0 ] || harness "seeding the alerts failed: $(err)"
[ "$(scalar "select string_agg(key || ':' || state, ' ' order by key) from spawn_alerts")" = "bb:a1:cleared bb:a2:cleared bb:a3:open bb:a4:open bb:a5:notice" ] ||
    harness "the seed folded to [$(scalar "select string_agg(key || ':' || state, ' ' order by key) from spawn_alerts")]"

# --- (1) the owner's verb ----------------------------------------------------------------

judge() { # <key> <verdict>: the verb's exit status
    "$SPAWN_BIN" alert judge "$1" "$2" >"$WORK/judge.out" 2>&1
}
judged_count() { scalar "select count(*) from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = 'alert.judged'"; }
if judge bb:a2 false; then :; else check_failed "(1) spawn alert judge bb:a2 false failed: $(head -c 300 "$WORK/judge.out")"; fi
if judge bb:a3 duplicate; then :; else check_failed "(1) spawn alert judge bb:a3 duplicate failed: $(head -c 300 "$WORK/judge.out")"; fi
expect_eq "$(judged_count)" 2 "(1) the two judgements are recorded (alert.judged)"
for refused in "bb:a1 wrong" "bb:none false" "bb:a2 stale" "bb:a5 false"; do
    # shellcheck disable=SC2086
    if judge $refused; then
        check_failed "(1) spawn alert judge $refused was taken: $(head -c 300 "$WORK/judge.out")"
    fi
done
expect_eq "$(judged_count)" 2 "(1) the refused judgements (another verdict, no such alert, judged already, a notice) wrote nothing"
note "(1) checked: the verb"

web=$WORK/web
mkdir -p "$web"
printf '<!doctype html><title>spawn</title>\n' >"$web/index.html"
"$SPAWN_BIN" serve --address "127.0.0.1:0" --web "$web" >"$WORK/serve.log" 2>&1 &
serve_pid=$!
port=""
i=0
while [ "$i" -lt 100 ]; do
    i=$((i + 1))
    port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$WORK/serve.log" 2>/dev/null | head -n 1)
    [ -n "$port" ] && break
    sleep 0.1
done
[ -n "$port" ] || harness "serve did not start: $(head -c 400 "$WORK/serve.log")"
get() { # <path> <file>: the answer's code
    curl -s --noproxy '*' -o "$2" -w '%{http_code}' "http://127.0.0.1:$port$1" 2>/dev/null || echo 000
}

# --- (2) the outcomes ----------------------------------------------------------------------

code=$(get /api/alerts "$WORK/api.json")
[ "$code" = 200 ] || harness "GET /api/alerts answered $code"
expect_eq "$(jq -c '[.[] | select(.kind == "bb_kind") | {key, state, outcome}] | sort_by(.key)' "$WORK/api.json")" \
    '[{"key":"bb:a1","state":"cleared","outcome":"cleared"},{"key":"bb:a2","state":"cleared","outcome":"false"},{"key":"bb:a3","state":"cleared","outcome":"duplicate"},{"key":"bb:a4","state":"open","outcome":null},{"key":"bb:a5","state":"notice","outcome":null}]' \
    "(2) each alert's outcome: cleared by its condition, judged false, judged a duplicate (and cleared by it), open, a notice"
expect_eq "$(jq -c '[.[] | select(.key == "bb:a3") | .cleared_evidence.judged]' "$WORK/api.json")" '["duplicate"]' \
    "(2) the open alert judged a duplicate: cleared by the judgement, its cleared evidence the verdict"
expect_eq "$(jq -c '[.[] | select(.key == "bb:a2") | .cleared_evidence.why]' "$WORK/api.json")" '["the condition went away"]' \
    "(2) the cleared alert judged false keeps the evidence it was cleared with"
note "(2) checked: the outcomes"

# --- (3) the north star --------------------------------------------------------------------

code=$(get /api/core-lines "$WORK/lines.json")
if [ "$code" != 200 ]; then
    check_failed "(3) GET /api/core-lines answered $code"
else
    got=$(jq -c '.lines[] | select(.line == "observability") | .north_star | {key, unit, value, state}' "$WORK/lines.json")
    expect_eq "$(printf '%s' "$got" | jq -c '{key, unit, state}')" '{"key":"observability.needs_you_right","unit":"share","state":"no_range"}' \
        "(3) the observability north star has a value and no range set"
    awk -v v="$(printf '%s' "$got" | jq -r '.value')" 'BEGIN { d = v - 1 / 3; if (d < 0) d = -d; exit !(v != "null" && d < 1e-9) }' ||
        check_failed "(3) Needs you is right [$(printf '%s' "$got" | jq -r '.value')]: one right (a1) of the three that ended (a1, a2 false, a3 duplicate)"
    note "(3) checked: the north star"
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "092: $(wc -l <"$WORK/failures") check(s) failed" >&2
    exit 1
fi
echo "092: every alert ends with an outcome, the owner judges a wrong one, and the card says how often Needs you was right"
exit 0
